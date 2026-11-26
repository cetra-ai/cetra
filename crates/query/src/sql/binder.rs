//! Shape B SQL AST binder and typechecker (`Statement` -> typed [`LogicalPlan`]).

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use cetra_column::{Batch, CancelToken, DataType, Field, ScalarValue, Schema};
use sqlparser::ast::{
    CastKind, DescribeAlias, Distinct, Expr as SqlExpr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, Ident, Join, JoinConstraint, JoinOperator, LimitClause,
    ObjectName, ObjectNamePart, OrderBy, OrderByExpr, OrderByKind, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, SetOperator, SetQuantifier, Spanned, Statement,
    TableAlias, TableAliasColumnDef, TableFactor, TableWithJoins, Value, Values, WindowFrameBound,
    WindowFrameUnits, WindowSpec as AstWindowSpec, WindowType,
};

use crate::bind::{
    ExprResolver, bind_expr_with, bind_sql_type, can_cast, coerce_common_many,
    coerce_comparison_many, coerce_expr, expect_bool_or_null,
};
use crate::error::{InvalidQuery, Position, QueryError};
use crate::eval::eval_expr;
use crate::exec::hash_agg::{AggExpr, AggFunc};
use crate::exec::hash_join::{JoinType, join_output_schema};
use crate::exec::sort_key::SortKey;
use crate::exec::sort_window::{FrameBound, WindowFunc, WindowSpec};
use crate::exec::source::TimeRangeHint;
use crate::ir::{BinaryOp, CastMode, EvalContext, Expr, ScalarFn, UnaryOp};
use crate::text::{DENIED_FUNCTIONS, position_of};

use super::catalog::{
    SqlCatalog, SqlTableDef, SqlViewKind, TimeWindow, raw_scores_schema, raw_spans_schema,
    raw_traces_schema, scores_view_schema, sessions_view_schema, spans_view_schema,
    traces_view_schema,
};
use super::logical::{LogicalJoinKind, LogicalPlan, explain_schema, unique_names};
use super::params::{Placeholder, SqlParamValue, coerce_param_value};

const MAX_AST_DEPTH: usize = 64;
const MAX_PLAN_NODES: usize = 4_096;

/// Parameter binding input for [`bind_statement`]: either declared [`Placeholder`] types
/// (for schema/`EXPLAIN` checks without runtime values) or concrete [`SqlParamValue`]s.
///
/// Implements a manual privacy-safe [`fmt::Debug`] omitting parameter values (`AGENTS.md` § Privacy).
#[derive(Clone, Copy)]
pub enum BoundParams<'a> {
    /// Declared parameter types from [`Placeholder`].
    Declared(&'a [Placeholder]),
    /// Bound runtime parameter values from [`super::params::parse_param_values`].
    Values(&'a [SqlParamValue]),
}

impl fmt::Debug for BoundParams<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Declared(phs) => f.debug_tuple("Declared").field(&phs.len()).finish(),
            Self::Values(vals) => f.debug_tuple("Values").field(&vals.len()).finish(),
        }
    }
}

/// Binds and typechecks a validated `sqlparser` [`Statement`] against `catalog` and `params`
/// into a typed Shape B [`LogicalPlan`].
pub fn bind_statement(
    stmt: &Statement,
    catalog: &SqlCatalog,
    params: &BoundParams<'_>,
) -> Result<LogicalPlan, QueryError> {
    let plan_nodes = Cell::new(0usize);
    let ctx = BindCtx {
        catalog,
        params,
        ctes: HashMap::new(),
        outer_scopes: Vec::new(),
        depth: 0,
        plan_nodes: &plan_nodes,
    };
    match stmt {
        Statement::Query(query) => {
            let plan = bind_query(query, &ctx)?;
            unique_names(plan.schema())?;
            Ok(plan)
        }
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
                return Err(
                    InvalidQuery::new("only plain EXPLAIN of a query is allowed", None).into(),
                );
            };
            let inner = bind_query(query, &ctx)?;
            unique_names(inner.schema())?;
            Ok(LogicalPlan::Explain {
                inner: Box::new(inner),
                schema: explain_schema(),
            })
        }
        Statement::Explain { .. } => {
            Err(InvalidQuery::new("only plain EXPLAIN of a query is allowed", None).into())
        }
        _ => Err(InvalidQuery::new("only SELECT queries are allowed", None).into()),
    }
}

// ---------------------------------------------------------------------------
// Context, CTEs, and Scope
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CteDef {
    plan: LogicalPlan,
    node_count: usize,
}

#[derive(Clone)]
struct BindCtx<'a> {
    catalog: &'a SqlCatalog,
    params: &'a BoundParams<'a>,
    ctes: HashMap<String, CteDef>,
    outer_scopes: Vec<&'a Scope>,
    depth: usize,
    plan_nodes: &'a Cell<usize>,
}

impl<'a> BindCtx<'a> {
    fn with_ctes(&self, ctes: HashMap<String, CteDef>) -> Self {
        Self {
            catalog: self.catalog,
            params: self.params,
            ctes,
            outer_scopes: self.outer_scopes.clone(),
            depth: self.depth,
            plan_nodes: self.plan_nodes,
        }
    }

    fn with_depth(&self, depth: usize) -> Self {
        Self {
            catalog: self.catalog,
            params: self.params,
            ctes: self.ctes.clone(),
            outer_scopes: self.outer_scopes.clone(),
            depth,
            plan_nodes: self.plan_nodes,
        }
    }

    fn child_with_outer<'b>(&'b self, outer: &'b Scope) -> BindCtx<'b> {
        let mut outer_scopes = Vec::with_capacity(self.outer_scopes.len() + 1);
        outer_scopes.extend_from_slice(&self.outer_scopes);
        outer_scopes.push(outer);
        BindCtx {
            catalog: self.catalog,
            params: self.params,
            ctes: self.ctes.clone(),
            outer_scopes,
            depth: self.depth + 1,
            plan_nodes: self.plan_nodes,
        }
    }

    fn record_plan_nodes(&self, nodes: usize, pos: Option<Position>) -> Result<(), QueryError> {
        let next = self.plan_nodes.get().saturating_add(nodes);
        if next > MAX_PLAN_NODES {
            return Err(InvalidQuery::new(
                format!("query plan exceeds maximum size of {MAX_PLAN_NODES} nodes"),
                pos,
            )
            .into());
        }
        self.plan_nodes.set(next);
        Ok(())
    }
}

#[derive(Clone)]
struct ScopeCol {
    qualifier: Option<String>,
    name: String,
    data_type: DataType,
    nullable: bool,
    index: usize,
    hidden_in_star: bool,
}

#[derive(Clone, Default)]
struct Scope {
    cols: Vec<ScopeCol>,
}

impl Scope {
    fn from_schema(schema: &Schema, qualifier: Option<&str>) -> Self {
        let cols = schema
            .fields()
            .iter()
            .enumerate()
            .map(|(index, field)| ScopeCol {
                qualifier: qualifier.map(str::to_owned),
                name: field.name.clone(),
                data_type: field.data_type.clone(),
                nullable: field.nullable,
                index,
                hidden_in_star: false,
            })
            .collect();
        Self { cols }
    }

    fn len(&self) -> usize {
        self.cols.len()
    }

    fn rename_prefix(&mut self, aliases: &[TableAliasColumnDef]) -> Result<(), InvalidQuery> {
        if aliases.len() > self.cols.len() {
            return Err(InvalidQuery::new(
                format!(
                    "table alias specifies {} columns, but relation has {}",
                    aliases.len(),
                    self.cols.len()
                ),
                at_ident(&aliases[0].name),
            ));
        }
        for (col, def) in self.cols.iter_mut().zip(aliases) {
            col.name = ident_canonical(&def.name);
        }
        Ok(())
    }

    fn set_qualifier(&mut self, qualifier: Option<&str>) {
        for col in &mut self.cols {
            col.qualifier = qualifier.map(str::to_owned);
        }
    }

    fn widen_nullability(&mut self, start: usize, end: usize) {
        for col in &mut self.cols[start..end] {
            col.nullable = true;
        }
    }

    fn resolve_local(&self, parts: &[Ident]) -> Result<Option<(usize, &ScopeCol)>, InvalidQuery> {
        match parts {
            [ident] => {
                let pos = at_ident(ident);
                let mut matches = self
                    .cols
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| !c.hidden_in_star && ident_matches(&c.name, ident));
                let Some(first) = matches.next() else {
                    return Ok(None);
                };
                if matches.next().is_some() {
                    return Err(InvalidQuery::new(
                        format!("ambiguous column `{}`", ident_display(ident)),
                        pos,
                    ));
                }
                Ok(Some(first))
            }
            [tbl, col] => {
                let pos = at_ident(col);
                let mut matches = self.cols.iter().enumerate().filter(|(_, c)| {
                    c.qualifier.as_ref().is_some_and(|q| ident_matches(q, tbl))
                        && ident_matches(&c.name, col)
                });
                let Some(first) = matches.next() else {
                    return Ok(None);
                };
                if matches.next().is_some() {
                    return Err(InvalidQuery::new(
                        format!(
                            "ambiguous column `{}.{}`",
                            ident_display(tbl),
                            ident_display(col)
                        ),
                        pos,
                    ));
                }
                Ok(Some(first))
            }
            [] => Err(InvalidQuery::new("empty identifier", None)),
            _ => Err(InvalidQuery::new(
                "qualified column names with more than two parts are not supported",
                at_ident(&parts[0]),
            )),
        }
    }

    fn resolve_ident(
        &self,
        parts: &[Ident],
        outer_scopes: &[&Scope],
    ) -> Result<Expr, InvalidQuery> {
        if let Some((_, col)) = self.resolve_local(parts)? {
            return Ok(Expr::Column {
                index: col.index,
                name: col.name.clone(),
                data_type: col.data_type.clone(),
            });
        }
        for outer in outer_scopes.iter().rev() {
            if let Ok(Some(_)) = outer.resolve_local(parts) {
                let label = parts
                    .iter()
                    .map(ident_display)
                    .collect::<Vec<_>>()
                    .join(".");
                return Err(InvalidQuery::new(
                    format!("correlated subqueries are not supported (outer reference `{label}`)"),
                    at_ident(&parts[0]),
                ));
            }
        }
        let label = parts
            .iter()
            .map(ident_display)
            .collect::<Vec<_>>()
            .join(".");
        Err(InvalidQuery::new(
            format!("unknown column `{label}`"),
            parts.first().and_then(at_ident),
        ))
    }
}

fn ident_canonical(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_ascii_lowercase()
    }
}

fn ident_display(ident: &Ident) -> String {
    ident_canonical(ident)
}

fn ident_matches(candidate: &str, ident: &Ident) -> bool {
    if ident.quote_style.is_some() {
        candidate == ident.value
    } else {
        candidate.eq_ignore_ascii_case(&ident.value)
    }
}

struct ScopeResolver<'a> {
    scope: &'a Scope,
    outer_scopes: &'a [&'a Scope],
    params: &'a BoundParams<'a>,
    prebound: &'a HashMap<String, ScalarValue>,
    /// Optional fallback aliases for `ORDER BY` expressions referencing `SELECT` output aliases.
    select_aliases: Option<&'a [(String, Expr)]>,
}

impl ExprResolver for ScopeResolver<'_> {
    fn resolve_ident(&self, parts: &[Ident]) -> Result<Expr, InvalidQuery> {
        match self.scope.resolve_ident(parts, self.outer_scopes) {
            Ok(expr) => Ok(expr),
            Err(err) => {
                if parts.len() == 1
                    && let Some(aliases) = self.select_aliases
                {
                    let ident = &parts[0];
                    let mut found = aliases
                        .iter()
                        .filter(|(name, _)| ident_matches(name, ident));
                    if let Some((_, expr)) = found.next() {
                        if found.next().is_some() {
                            return Err(InvalidQuery::new(
                                format!("ambiguous column `{}`", ident_display(ident)),
                                at_ident(ident),
                            ));
                        }
                        return Ok(expr.clone());
                    }
                }
                Err(err)
            }
        }
    }

    fn resolve_placeholder(&self, name: &str, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
        if let Some(sv) = self.prebound.get(name) {
            return Ok(Expr::Literal(sv.clone()));
        }
        let idx = parse_placeholder_index(name, pos)?;
        match self.params {
            BoundParams::Values(vals) => {
                let param = vals
                    .get(idx)
                    .ok_or_else(|| InvalidQuery::new(format!("unknown placeholder {name}"), pos))?;
                Ok(Expr::Literal(param.value.clone()))
            }
            BoundParams::Declared(phs) => {
                let ph = phs
                    .get(idx)
                    .ok_or_else(|| InvalidQuery::new(format!("unknown placeholder {name}"), pos))?;
                Ok(Expr::Literal(ScalarValue::Null(ph.kind.data_type())))
            }
        }
    }
}

fn parse_placeholder_index(name: &str, pos: Option<Position>) -> Result<usize, InvalidQuery> {
    let Some(rest) = name.strip_prefix('$') else {
        return Err(InvalidQuery::new(
            format!("invalid placeholder `{name}`"),
            pos,
        ));
    };
    let one_based: usize = rest
        .parse()
        .map_err(|_| InvalidQuery::new(format!("invalid placeholder `{name}`"), pos))?;
    one_based
        .checked_sub(1)
        .ok_or_else(|| InvalidQuery::new(format!("invalid placeholder `{name}`"), pos))
}

fn lookup_param_info<'a>(
    params: &'a BoundParams<'a>,
    name: &str,
    pos: Option<Position>,
) -> Result<(String, DataType, Option<&'a SqlParamValue>), InvalidQuery> {
    let idx = parse_placeholder_index(name, pos)?;
    match params {
        BoundParams::Values(vals) => {
            let v = vals
                .get(idx)
                .ok_or_else(|| InvalidQuery::new(format!("unknown placeholder {name}"), pos))?;
            Ok((v.name.clone(), v.value.data_type(), Some(v)))
        }
        BoundParams::Declared(phs) => {
            let p = phs
                .get(idx)
                .ok_or_else(|| InvalidQuery::new(format!("unknown placeholder {name}"), pos))?;
            Ok((p.name.clone(), p.kind.data_type(), None))
        }
    }
}

fn coerce_placeholder_to(
    params: &BoundParams<'_>,
    name: &str,
    target: &DataType,
    pos: Option<Position>,
) -> Result<ScalarValue, QueryError> {
    let (param_name, declared_dt, maybe_val) = lookup_param_info(params, name, pos)?;
    if let Some(val) = maybe_val {
        return coerce_param_value(val, target);
    }
    if declared_dt == *target {
        return Ok(ScalarValue::Null(target.clone()));
    }
    if !can_cast(&declared_dt, target) {
        return Err(QueryError::invalid_request(format!(
            "parameter {param_name} does not fit where it is used ({})",
            target.display_name()
        )));
    }
    Ok(ScalarValue::Null(target.clone()))
}

// ---------------------------------------------------------------------------
// Scalar Expression Binding with Usage-Site Parameter Coercion
// ---------------------------------------------------------------------------

fn bind_sql_expr_with_prebound(
    ast: &SqlExpr,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    select_aliases: Option<&[(String, Expr)]>,
    prebound: &HashMap<String, ScalarValue>,
) -> Result<Expr, QueryError> {
    let resolver = ScopeResolver {
        scope,
        outer_scopes: &ctx.outer_scopes,
        params: ctx.params,
        prebound,
        select_aliases,
    };
    bind_expr_with(ast, &resolver).map_err(QueryError::from)
}

fn clone_sql_expr(expr: &SqlExpr) -> SqlExpr {
    let mut spine = Vec::new();
    let mut cur = expr;
    while let SqlExpr::BinaryOp { left, op, right } = cur {
        spine.push((op.clone(), right.as_ref()));
        cur = left.as_ref();
    }
    let mut acc = cur.clone();
    for (op, right) in spine.into_iter().rev() {
        let right_cloned = if matches!(right, SqlExpr::BinaryOp { .. }) {
            clone_sql_expr(right)
        } else {
            right.clone()
        };
        acc = SqlExpr::BinaryOp {
            left: Box::new(acc),
            op,
            right: Box::new(right_cloned),
        };
    }
    acc
}

fn bind_sql_expr(
    ast: &SqlExpr,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    select_aliases: Option<&[(String, Expr)]>,
) -> Result<Expr, QueryError> {
    if !contains_placeholder(ast) {
        let prebound = HashMap::new();
        return bind_sql_expr_with_prebound(ast, scope, ctx, select_aliases, &prebound);
    }

    let mut prebound = HashMap::new();
    let mut counter = 0usize;
    let mut cloned = clone_sql_expr(ast);
    precoerce_expr(
        &mut cloned,
        None,
        scope,
        ctx,
        select_aliases,
        &mut prebound,
        &mut counter,
        0,
    )?;
    bind_sql_expr_with_prebound(&cloned, scope, ctx, select_aliases, &prebound)
}

fn bind_bool_sql_expr(ast: &SqlExpr, scope: &Scope, ctx: &BindCtx<'_>) -> Result<Expr, QueryError> {
    let pos = at(ast);
    if let Some((ph_name, ph_pos)) = unwrap_placeholder(ast) {
        let coerced = coerce_placeholder_to(ctx.params, ph_name, &DataType::Bool, ph_pos)?;
        return Ok(Expr::Literal(coerced));
    }
    let expr = bind_sql_expr(ast, scope, ctx, None)?;
    expect_bool_or_null(expr, pos).map_err(QueryError::from)
}

fn contains_placeholder(ast: &SqlExpr) -> bool {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        match node {
            SqlExpr::Value(vws) => {
                if matches!(vws.value, Value::Placeholder(_)) {
                    return true;
                }
            }
            SqlExpr::Nested(inner)
            | SqlExpr::UnaryOp { expr: inner, .. }
            | SqlExpr::IsNull(inner)
            | SqlExpr::IsNotNull(inner)
            | SqlExpr::IsUnknown(inner)
            | SqlExpr::IsNotUnknown(inner)
            | SqlExpr::IsTrue(inner)
            | SqlExpr::IsFalse(inner)
            | SqlExpr::IsNotTrue(inner)
            | SqlExpr::IsNotFalse(inner)
            | SqlExpr::Cast { expr: inner, .. }
            | SqlExpr::Ceil { expr: inner, .. }
            | SqlExpr::Floor { expr: inner, .. }
            | SqlExpr::Extract { expr: inner, .. } => stack.push(inner),
            SqlExpr::BinaryOp { left, right, .. }
            | SqlExpr::IsDistinctFrom(left, right)
            | SqlExpr::IsNotDistinctFrom(left, right)
            | SqlExpr::Position {
                expr: left,
                r#in: right,
            } => {
                stack.push(left);
                stack.push(right);
            }
            SqlExpr::Between {
                expr, low, high, ..
            } => {
                stack.push(expr);
                stack.push(low);
                stack.push(high);
            }
            SqlExpr::InList { expr, list, .. } => {
                stack.push(expr);
                stack.extend(list.iter());
            }
            SqlExpr::Like { expr, pattern, .. } | SqlExpr::ILike { expr, pattern, .. } => {
                stack.push(expr);
                stack.push(pattern);
            }
            SqlExpr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(op) = operand {
                    stack.push(op);
                }
                for cw in conditions {
                    stack.push(&cw.condition);
                    stack.push(&cw.result);
                }
                if let Some(el) = else_result {
                    stack.push(el);
                }
            }
            SqlExpr::Array(arr) => stack.extend(arr.elem.iter()),
            SqlExpr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                stack.push(expr);
                if let Some(f) = substring_from {
                    stack.push(f);
                }
                if let Some(l) = substring_for {
                    stack.push(l);
                }
            }
            SqlExpr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                stack.push(expr);
                if let Some(w) = trim_what {
                    stack.push(w);
                }
                if let Some(chars) = trim_characters {
                    stack.extend(chars.iter());
                }
            }
            SqlExpr::CompoundFieldAccess { root, access_chain } => {
                stack.push(root);
                for acc in access_chain {
                    if let sqlparser::ast::AccessExpr::Subscript(
                        sqlparser::ast::Subscript::Index { index },
                    ) = acc
                    {
                        stack.push(index);
                    }
                }
            }
            SqlExpr::JsonAccess { value, path } => {
                stack.push(value);
                for elem in &path.path {
                    if let sqlparser::ast::JsonPathElem::Bracket { key }
                    | sqlparser::ast::JsonPathElem::ColonBracket { key } = elem
                    {
                        stack.push(key);
                    }
                }
            }
            SqlExpr::Function(f) => {
                if let FunctionArguments::List(list) = &f.args {
                    for arg in &list.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            stack.push(e);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn unwrap_placeholder(ast: &SqlExpr) -> Option<(&str, Option<Position>)> {
    let mut cur = ast;
    while let SqlExpr::Nested(inner) = cur {
        cur = inner;
    }
    if let SqlExpr::Value(vws) = cur
        && let Value::Placeholder(name) = &vws.value
    {
        return Some((name.as_str(), at(cur)));
    }
    None
}

fn unwrap_placeholder_mut(ast: &mut SqlExpr) -> Option<(&mut String, Option<Position>)> {
    let mut cur = ast;
    while let SqlExpr::Nested(inner) = cur {
        cur = inner;
    }
    let pos = at(cur);
    if let SqlExpr::Value(vws) = cur
        && let Value::Placeholder(name) = &mut vws.value
    {
        return Some((name, pos));
    }
    None
}

fn needs_usage_coercion(declared_dt: &DataType, target_dt: &DataType) -> bool {
    if declared_dt == target_dt || *target_dt == DataType::Null {
        return false;
    }
    if *declared_dt == DataType::Utf8 {
        return true;
    }
    !matches!(
        (declared_dt, target_dt),
        (DataType::I64, DataType::F64)
            | (DataType::F64, DataType::I64)
            | (DataType::TimestampNs { .. }, DataType::TimestampNs { .. })
    )
}

fn alloc_prebound(
    target_slot: &mut String,
    val: ScalarValue,
    prebound: &mut HashMap<String, ScalarValue>,
    counter: &mut usize,
) {
    let key = format!("$p{counter}");
    *counter += 1;
    prebound.insert(key.clone(), val);
    *target_slot = key;
}

#[expect(clippy::too_many_arguments)]
fn precoerce_expr(
    ast: &mut SqlExpr,
    expected: Option<&DataType>,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    select_aliases: Option<&[(String, Expr)]>,
    prebound: &mut HashMap<String, ScalarValue>,
    counter: &mut usize,
    depth: usize,
) -> Result<(), QueryError> {
    if depth > MAX_AST_DEPTH {
        return Err(InvalidQuery::new("the expression is nested too deeply", at(ast)).into());
    }
    if !contains_placeholder(ast) {
        return Ok(());
    }

    if let Some(target_dt) = expected
        && let Some((name, pos)) = unwrap_placeholder_mut(ast)
        && !prebound.contains_key(name.as_str())
    {
        let (_, decl_dt, _) = lookup_param_info(ctx.params, name, pos)?;
        if needs_usage_coercion(&decl_dt, target_dt) {
            let coerced = coerce_placeholder_to(ctx.params, name, target_dt, pos)?;
            alloc_prebound(name, coerced, prebound, counter);
        }
        return Ok(());
    }

    match ast {
        SqlExpr::Nested(inner) => {
            precoerce_expr(
                inner,
                expected,
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::IsNull(inner)
        | SqlExpr::IsNotNull(inner)
        | SqlExpr::IsUnknown(inner)
        | SqlExpr::IsNotUnknown(inner) => {
            precoerce_expr(
                inner,
                None,
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::IsTrue(inner)
        | SqlExpr::IsFalse(inner)
        | SqlExpr::IsNotTrue(inner)
        | SqlExpr::IsNotFalse(inner) => {
            precoerce_expr(
                inner,
                Some(&DataType::Bool),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            array: false,
            format: None,
        } => {
            if let Some((name, pos)) = unwrap_placeholder_mut(expr)
                && !prebound.contains_key(name.as_str())
            {
                let target = bind_sql_type(data_type, pos)?;
                let coerced = coerce_placeholder_to(ctx.params, name, &target, pos)?;
                alloc_prebound(name, coerced, prebound, counter);
            } else {
                precoerce_expr(
                    expr,
                    None,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
        }
        SqlExpr::Cast {
            kind: CastKind::TryCast | CastKind::SafeCast,
            expr,
            array: false,
            format: None,
            ..
        } => {
            precoerce_expr(
                expr,
                None,
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::Ceil { expr, .. } | SqlExpr::Floor { expr, .. } => {
            precoerce_expr(
                expr,
                Some(&DataType::F64),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::Extract { expr, .. } => {
            precoerce_expr(
                expr,
                Some(&DataType::timestamp_utc()),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::JsonAccess { value, path } => {
            precoerce_expr(
                value,
                Some(&DataType::Utf8),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
            for elem in &mut path.path {
                if let sqlparser::ast::JsonPathElem::Bracket { key }
                | sqlparser::ast::JsonPathElem::ColonBracket { key } = elem
                {
                    precoerce_expr(
                        key,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth + 1,
                    )?;
                }
            }
        }
        SqlExpr::UnaryOp { op, expr } => {
            let child_expected = match op {
                sqlparser::ast::UnaryOperator::Not => Some(&DataType::Bool),
                sqlparser::ast::UnaryOperator::Minus | sqlparser::ast::UnaryOperator::Plus => {
                    expected
                }
                _ => None,
            };
            precoerce_expr(
                expr,
                child_expected,
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::BinaryOp { .. } => {
            precoerce_binary_spine(ast, scope, ctx, select_aliases, prebound, counter, depth)?;
        }
        SqlExpr::IsDistinctFrom(left, right) | SqlExpr::IsNotDistinctFrom(left, right) => {
            precoerce_pair(
                left,
                right,
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::Between {
            expr, low, high, ..
        } => {
            let pos = at(expr.as_ref());
            if unwrap_placeholder(expr).is_none() {
                if contains_placeholder(expr) {
                    precoerce_expr(
                        expr,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth + 1,
                    )?;
                }
                let dt = bind_sql_expr_with_prebound(expr, scope, ctx, select_aliases, prebound)?
                    .data_type();
                precoerce_expr(
                    low,
                    Some(&dt),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
                precoerce_expr(
                    high,
                    Some(&dt),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            } else {
                let mut bound_bounds = Vec::new();
                for bound_ast in [&mut *low, &mut *high] {
                    if unwrap_placeholder(bound_ast).is_none() {
                        if contains_placeholder(bound_ast) {
                            precoerce_expr(
                                bound_ast,
                                None,
                                scope,
                                ctx,
                                select_aliases,
                                prebound,
                                counter,
                                depth + 1,
                            )?;
                        }
                        bound_bounds.push(bind_sql_expr_with_prebound(
                            bound_ast,
                            scope,
                            ctx,
                            select_aliases,
                            prebound,
                        )?);
                    }
                }
                let target_dt = if bound_bounds.is_empty() {
                    None
                } else {
                    let refs: Vec<&Expr> = bound_bounds.iter().collect();
                    let common = coerce_comparison_many(&refs, pos)?;
                    (common != DataType::Null).then_some(common)
                };
                precoerce_expr(
                    expr,
                    target_dt.as_ref(),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
                precoerce_expr(
                    low,
                    target_dt.as_ref(),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
                precoerce_expr(
                    high,
                    target_dt.as_ref(),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
        }
        SqlExpr::InList { expr, list, .. } => {
            let pos = at(expr.as_ref());
            if unwrap_placeholder(expr).is_none() {
                if contains_placeholder(expr) {
                    precoerce_expr(
                        expr,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth + 1,
                    )?;
                }
                let dt = bind_sql_expr_with_prebound(expr, scope, ctx, select_aliases, prebound)?
                    .data_type();
                for item in list {
                    precoerce_expr(
                        item,
                        Some(&dt),
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth + 1,
                    )?;
                }
            } else {
                let mut bound_items = Vec::new();
                for item in list.iter_mut() {
                    if unwrap_placeholder(item).is_none() {
                        if contains_placeholder(item) {
                            precoerce_expr(
                                item,
                                None,
                                scope,
                                ctx,
                                select_aliases,
                                prebound,
                                counter,
                                depth + 1,
                            )?;
                        }
                        bound_items.push(bind_sql_expr_with_prebound(
                            item,
                            scope,
                            ctx,
                            select_aliases,
                            prebound,
                        )?);
                    }
                }
                let target_dt = if bound_items.is_empty() {
                    None
                } else {
                    let refs: Vec<&Expr> = bound_items.iter().collect();
                    let common = coerce_comparison_many(&refs, pos)?;
                    (common != DataType::Null).then_some(common)
                };
                precoerce_expr(
                    expr,
                    target_dt.as_ref(),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
                for item in list {
                    if unwrap_placeholder(item).is_some() {
                        precoerce_expr(
                            item,
                            target_dt.as_ref(),
                            scope,
                            ctx,
                            select_aliases,
                            prebound,
                            counter,
                            depth + 1,
                        )?;
                    }
                }
            }
        }
        SqlExpr::Like { expr, pattern, .. } | SqlExpr::ILike { expr, pattern, .. } => {
            precoerce_expr(
                expr,
                Some(&DataType::Utf8),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
            precoerce_expr(
                pattern,
                Some(&DataType::Utf8),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let op_dt = if let Some(op) = operand {
                if !contains_placeholder(op) {
                    Some(bind_sql_expr(op, scope, ctx, select_aliases)?.data_type())
                } else {
                    precoerce_expr(
                        op,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth + 1,
                    )?;
                    None
                }
            } else {
                None
            };
            for cw in conditions {
                let cond_target = if operand.is_some() {
                    op_dt.as_ref()
                } else {
                    Some(&DataType::Bool)
                };
                precoerce_expr(
                    &mut cw.condition,
                    cond_target,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
                precoerce_expr(
                    &mut cw.result,
                    None,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
            if let Some(el) = else_result {
                precoerce_expr(
                    el,
                    None,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
        }
        SqlExpr::CompoundFieldAccess { root, access_chain } => {
            let root_dt = if !contains_placeholder(root) {
                Some(bind_sql_expr(root, scope, ctx, select_aliases)?.data_type())
            } else {
                precoerce_expr(
                    root,
                    None,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
                None
            };
            for (i, acc) in access_chain.iter_mut().enumerate() {
                if let sqlparser::ast::AccessExpr::Subscript(sqlparser::ast::Subscript::Index {
                    index,
                }) = acc
                {
                    let idx_target = if i == 0 {
                        match root_dt {
                            Some(DataType::MapUtf8) => Some(DataType::Utf8),
                            Some(DataType::List(_)) => Some(DataType::I64),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    precoerce_expr(
                        index,
                        idx_target.as_ref(),
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth + 1,
                    )?;
                }
            }
        }
        SqlExpr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            precoerce_expr(
                expr,
                Some(&DataType::Utf8),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
            if let Some(f) = substring_from {
                precoerce_expr(
                    f,
                    Some(&DataType::I64),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
            if let Some(l) = substring_for {
                precoerce_expr(
                    l,
                    Some(&DataType::I64),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
        }
        SqlExpr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            precoerce_expr(
                expr,
                Some(&DataType::Utf8),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
            if let Some(w) = trim_what {
                precoerce_expr(
                    w,
                    Some(&DataType::Utf8),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
            if let Some(chars) = trim_characters {
                for ch in chars {
                    precoerce_expr(
                        ch,
                        Some(&DataType::Utf8),
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth + 1,
                    )?;
                }
            }
        }
        SqlExpr::Position { expr, r#in } => {
            precoerce_expr(
                expr,
                Some(&DataType::Utf8),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
            precoerce_expr(
                r#in,
                Some(&DataType::Utf8),
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::Function(f) => {
            precoerce_function(
                f,
                expected,
                scope,
                ctx,
                select_aliases,
                prebound,
                counter,
                depth + 1,
            )?;
        }
        SqlExpr::Array(arr) => {
            for el in &mut arr.elem {
                precoerce_expr(
                    el,
                    None,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth + 1,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn can_receive_expected(mut expr: &SqlExpr) -> bool {
    loop {
        match expr {
            SqlExpr::Nested(inner) => expr = inner,
            SqlExpr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Minus | sqlparser::ast::UnaryOperator::Plus,
                expr: inner,
            } => expr = inner,
            SqlExpr::Value(vws) => return matches!(vws.value, Value::Placeholder(_)),
            SqlExpr::Function(f) => {
                return bare_object_name(&f.name).is_some_and(|n| {
                    matches!(
                        n.as_str(),
                        "coalesce" | "nullif" | "greatest" | "least" | "if"
                    )
                });
            }
            _ => return false,
        }
    }
}

fn precoerce_binary_spine(
    ast: &mut SqlExpr,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    select_aliases: Option<&[(String, Expr)]>,
    prebound: &mut HashMap<String, ScalarValue>,
    counter: &mut usize,
    depth: usize,
) -> Result<(), QueryError> {
    // Walk left-deep binary spines iteratively so 512-op chains never overflow the stack
    // or trigger O(N^2) re-scanning.
    let mut work: Vec<(&mut SqlExpr, usize)> = vec![(ast, depth)];
    while let Some((node, d)) = work.pop() {
        if d > MAX_AST_DEPTH {
            return Err(InvalidQuery::new("the expression is nested too deeply", at(node)).into());
        }
        let SqlExpr::BinaryOp { left, op, right } = node else {
            continue;
        };
        use sqlparser::ast::BinaryOperator as BO;
        match op {
            BO::And | BO::Or => {
                if let Some((name, pos)) = unwrap_placeholder_mut(right) {
                    if !prebound.contains_key(name.as_str()) {
                        let coerced =
                            coerce_placeholder_to(ctx.params, name, &DataType::Bool, pos)?;
                        alloc_prebound(name, coerced, prebound, counter);
                    }
                } else if matches!(right.as_ref(), SqlExpr::BinaryOp { .. }) {
                    work.push((right, d + 1));
                } else {
                    precoerce_expr(
                        right,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        d + 1,
                    )?;
                }

                if let Some((name, pos)) = unwrap_placeholder_mut(left) {
                    if !prebound.contains_key(name.as_str()) {
                        let coerced =
                            coerce_placeholder_to(ctx.params, name, &DataType::Bool, pos)?;
                        alloc_prebound(name, coerced, prebound, counter);
                    }
                } else if matches!(left.as_ref(), SqlExpr::BinaryOp { .. }) {
                    work.push((left, d));
                } else {
                    precoerce_expr(
                        left,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        d + 1,
                    )?;
                }
            }
            BO::Eq
            | BO::NotEq
            | BO::Lt
            | BO::LtEq
            | BO::Gt
            | BO::GtEq
            | BO::Plus
            | BO::Minus
            | BO::Multiply
            | BO::Divide
            | BO::Modulo => {
                let left_expects = can_receive_expected(left);
                let right_expects = can_receive_expected(right);
                if left_expects && contains_placeholder(left) && !contains_placeholder(right) {
                    let r_dt =
                        bind_sql_expr_with_prebound(right, scope, ctx, select_aliases, prebound)?
                            .data_type();
                    precoerce_expr(
                        left,
                        Some(&r_dt),
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        d + 1,
                    )?;
                } else if right_expects
                    && contains_placeholder(right)
                    && !contains_placeholder(left)
                {
                    let l_dt =
                        bind_sql_expr_with_prebound(left, scope, ctx, select_aliases, prebound)?
                            .data_type();
                    precoerce_expr(
                        right,
                        Some(&l_dt),
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        d + 1,
                    )?;
                } else {
                    if matches!(right.as_ref(), SqlExpr::BinaryOp { .. }) {
                        work.push((right, d + 1));
                    } else {
                        precoerce_expr(
                            right,
                            None,
                            scope,
                            ctx,
                            select_aliases,
                            prebound,
                            counter,
                            d + 1,
                        )?;
                    }
                    if matches!(left.as_ref(), SqlExpr::BinaryOp { .. }) {
                        work.push((left, d));
                    } else {
                        precoerce_expr(
                            left,
                            None,
                            scope,
                            ctx,
                            select_aliases,
                            prebound,
                            counter,
                            d + 1,
                        )?;
                    }
                }
            }
            _ => {
                if matches!(right.as_ref(), SqlExpr::BinaryOp { .. }) {
                    work.push((right, d + 1));
                } else {
                    precoerce_expr(
                        right,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        d + 1,
                    )?;
                }
                if matches!(left.as_ref(), SqlExpr::BinaryOp { .. }) {
                    work.push((left, d));
                } else {
                    precoerce_expr(
                        left,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        d + 1,
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn precoerce_pair(
    left: &mut SqlExpr,
    right: &mut SqlExpr,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    select_aliases: Option<&[(String, Expr)]>,
    prebound: &mut HashMap<String, ScalarValue>,
    counter: &mut usize,
    depth: usize,
) -> Result<(), QueryError> {
    let left_is_ph = unwrap_placeholder(left).is_some();
    let right_is_ph = unwrap_placeholder(right).is_some();
    if left_is_ph && !contains_placeholder(right) {
        let r_dt =
            bind_sql_expr_with_prebound(right, scope, ctx, select_aliases, prebound)?.data_type();
        precoerce_expr(
            left,
            Some(&r_dt),
            scope,
            ctx,
            select_aliases,
            prebound,
            counter,
            depth,
        )?;
    } else if right_is_ph && !contains_placeholder(left) {
        let l_dt =
            bind_sql_expr_with_prebound(left, scope, ctx, select_aliases, prebound)?.data_type();
        precoerce_expr(
            right,
            Some(&l_dt),
            scope,
            ctx,
            select_aliases,
            prebound,
            counter,
            depth,
        )?;
    } else {
        precoerce_expr(
            left,
            None,
            scope,
            ctx,
            select_aliases,
            prebound,
            counter,
            depth,
        )?;
        precoerce_expr(
            right,
            None,
            scope,
            ctx,
            select_aliases,
            prebound,
            counter,
            depth,
        )?;
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn precoerce_function(
    f: &mut Function,
    expected: Option<&DataType>,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    select_aliases: Option<&[(String, Expr)]>,
    prebound: &mut HashMap<String, ScalarValue>,
    counter: &mut usize,
    depth: usize,
) -> Result<(), QueryError> {
    let pos = at(f);
    let fn_name = bare_object_name(&f.name).unwrap_or_default();
    let FunctionArguments::List(list) = &mut f.args else {
        return Ok(());
    };
    if matches!(
        fn_name.as_str(),
        "coalesce" | "nullif" | "greatest" | "least"
    ) {
        let mut bound_args = Vec::new();
        for arg in &mut list.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
                continue;
            };
            if unwrap_placeholder(expr).is_none() {
                if contains_placeholder(expr) {
                    precoerce_expr(
                        expr,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth,
                    )?;
                }
                bound_args.push(bind_sql_expr_with_prebound(
                    expr,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                )?);
            }
        }
        let target: Option<DataType> = if bound_args.is_empty() {
            expected.cloned()
        } else {
            let refs: Vec<&Expr> = bound_args.iter().collect();
            let common = if fn_name == "coalesce" {
                coerce_common_many(&refs, pos)?
            } else {
                coerce_comparison_many(&refs, pos)?
            };
            if common != DataType::Null {
                Some(common)
            } else {
                expected.cloned()
            }
        };
        for arg in &mut list.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
                continue;
            };
            if unwrap_placeholder(expr).is_some() {
                precoerce_expr(
                    expr,
                    target.as_ref(),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth,
                )?;
            }
        }
        return Ok(());
    }
    if fn_name == "if" {
        let mut bound_branches = Vec::new();
        for (idx, arg) in list.args.iter_mut().enumerate() {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
                continue;
            };
            if idx == 0 {
                precoerce_expr(
                    expr,
                    Some(&DataType::Bool),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth,
                )?;
            } else if unwrap_placeholder(expr).is_none() {
                if contains_placeholder(expr) {
                    precoerce_expr(
                        expr,
                        None,
                        scope,
                        ctx,
                        select_aliases,
                        prebound,
                        counter,
                        depth,
                    )?;
                }
                bound_branches.push(bind_sql_expr_with_prebound(
                    expr,
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                )?);
            }
        }
        let branch_target: Option<DataType> = if bound_branches.is_empty() {
            expected.cloned()
        } else {
            let refs: Vec<&Expr> = bound_branches.iter().collect();
            let common = coerce_common_many(&refs, pos)?;
            if common != DataType::Null {
                Some(common)
            } else {
                expected.cloned()
            }
        };
        for arg in list.args.iter_mut().skip(1) {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
                continue;
            };
            if unwrap_placeholder(expr).is_some() {
                precoerce_expr(
                    expr,
                    branch_target.as_ref(),
                    scope,
                    ctx,
                    select_aliases,
                    prebound,
                    counter,
                    depth,
                )?;
            }
        }
        return Ok(());
    }
    for (idx, arg) in list.args.iter_mut().enumerate() {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
            continue;
        };
        let target: Option<DataType> = match (fn_name.as_str(), idx) {
            ("lower" | "upper" | "length" | "char_length" | "character_length", 0)
            | ("substr" | "substring" | "trim" | "ltrim" | "rtrim" | "left" | "right", 0)
            | ("split_part", 0 | 1)
            | ("starts_with" | "ends_with" | "contains" | "strpos", 0 | 1)
            | ("regexp_like" | "regexp_extract", 0..=2)
            | ("encode", 1)
            | ("date_trunc", 0)
            | ("json_extract" | "json_extract_text", 0 | 1) => Some(DataType::Utf8),
            ("date_trunc", 1) => Some(DataType::timestamp_utc()),
            ("substr" | "substring", 1 | 2)
            | ("left" | "right" | "round" | "array_length" | "array_element", 1)
            | ("split_part", 2) => Some(DataType::I64),
            ("sqrt" | "ln" | "log10" | "power" | "pow" | "isnan" | "isfinite", _) => {
                Some(DataType::F64)
            }
            _ => None,
        };
        precoerce_expr(
            expr,
            target.as_ref(),
            scope,
            ctx,
            select_aliases,
            prebound,
            counter,
            depth,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Query, CTEs, Set Operations, VALUES
// ---------------------------------------------------------------------------

fn bind_query(query: &Query, ctx: &BindCtx<'_>) -> Result<LogicalPlan, QueryError> {
    let pos = at(query);
    if ctx.depth > MAX_AST_DEPTH {
        return Err(InvalidQuery::new("the query is nested too deeply", pos).into());
    }
    if query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return Err(InvalidQuery::new("unsupported query clause", pos).into());
    }

    let mut cte_ctx = ctx.clone();
    if let Some(with) = &query.with {
        if with.recursive {
            return Err(InvalidQuery::new("recursive queries are not allowed", pos).into());
        }
        let mut ctes = ctx.ctes.clone();
        let mut local_names = HashSet::new();
        for cte in &with.cte_tables {
            let key = ident_canonical(&cte.alias.name);
            if !local_names.insert(key.clone()) {
                return Err(InvalidQuery::new(
                    format!("duplicate CTE name `{key}`"),
                    at_ident(&cte.alias.name),
                )
                .into());
            }
            let sub_ctx = cte_ctx.with_ctes(ctes.clone()).with_depth(ctx.depth + 1);
            let mut plan = bind_query(&cte.query, &sub_ctx)?;
            if !cte.alias.columns.is_empty() {
                plan = apply_column_aliases(plan, &cte.alias.columns)?;
            }
            let node_count = plan_node_count(&plan);
            if node_count > MAX_PLAN_NODES {
                return Err(InvalidQuery::new(
                    format!("query plan exceeds maximum size of {MAX_PLAN_NODES} nodes"),
                    at(cte.query.as_ref()),
                )
                .into());
            }
            ctes.insert(key, CteDef { plan, node_count });
        }
        cte_ctx = cte_ctx.with_ctes(ctes);
    }

    let body_ctx = cte_ctx.with_depth(ctx.depth + 1);
    let (plan, post_select_ctx) = bind_set_expr(&query.body, &body_ctx, query.order_by.as_ref())?;

    let plan = if let Some(order_by) = &query.order_by
        && !post_select_ctx.order_already_applied
    {
        apply_order_by_plain(plan, order_by, &cte_ctx)?
    } else {
        plan
    };

    let plan = if let Some(limit_clause) = &query.limit_clause {
        apply_limit_clause(plan, limit_clause, &cte_ctx)?
    } else {
        plan
    };

    if let Some(trim_indices) = post_select_ctx.trim_to_first_n {
        let in_schema = plan.schema();
        let mut exprs = Vec::with_capacity(trim_indices);
        let mut fields = Vec::with_capacity(trim_indices);
        for (i, f) in in_schema.fields().iter().take(trim_indices).enumerate() {
            exprs.push(Expr::Column {
                index: i,
                name: f.name.clone(),
                data_type: f.data_type.clone(),
            });
            fields.push(f.clone());
        }
        let schema = Schema::try_new_arc(fields)?;
        return Ok(LogicalPlan::Project {
            input: Box::new(plan),
            exprs,
            schema,
        });
    }

    Ok(plan)
}

struct PostSelectState {
    order_already_applied: bool,
    trim_to_first_n: Option<usize>,
}

fn bind_set_expr(
    body: &SetExpr,
    ctx: &BindCtx<'_>,
    outer_order_by: Option<&OrderBy>,
) -> Result<(LogicalPlan, PostSelectState), QueryError> {
    if ctx.depth > MAX_AST_DEPTH {
        return Err(InvalidQuery::new("the query is nested too deeply", at(body)).into());
    }
    let next_ctx = ctx.with_depth(ctx.depth + 1);
    match body {
        SetExpr::Select(select) => bind_select(select, &next_ctx, outer_order_by),
        SetExpr::Query(sub) => {
            let plan = bind_query(sub, &next_ctx)?;
            Ok((
                plan,
                PostSelectState {
                    order_already_applied: false,
                    trim_to_first_n: None,
                },
            ))
        }
        SetExpr::Values(values) => {
            let plan = bind_values(values, &next_ctx)?;
            Ok((
                plan,
                PostSelectState {
                    order_already_applied: false,
                    trim_to_first_n: None,
                },
            ))
        }
        SetExpr::SetOperation { .. } => {
            let plan = bind_set_operation_chain(body, &next_ctx)?;
            Ok((
                plan,
                PostSelectState {
                    order_already_applied: false,
                    trim_to_first_n: None,
                },
            ))
        }
        _ => Err(InvalidQuery::new("only SELECT queries are allowed", at(body)).into()),
    }
}

fn bind_set_operation_chain(body: &SetExpr, ctx: &BindCtx<'_>) -> Result<LogicalPlan, QueryError> {
    let mut chain = Vec::new();
    let mut cur = body;
    while let SetExpr::SetOperation {
        left,
        op,
        set_quantifier,
        right,
    } = cur
    {
        chain.push((op, set_quantifier, right.as_ref(), at(right.as_ref())));
        cur = left.as_ref();
    }

    let (mut acc, _) = bind_set_expr(cur, ctx, None)?;
    for (op, quantifier, right_ast, pos) in chain.into_iter().rev() {
        if matches!(
            quantifier,
            SetQuantifier::ByName | SetQuantifier::AllByName | SetQuantifier::DistinctByName
        ) {
            return Err(InvalidQuery::new("BY NAME set operations are not supported", pos).into());
        }
        let (right_plan, _) = bind_set_expr(right_ast, ctx, None)?;
        acc = combine_set_plans(acc, right_plan, *op, *quantifier, pos)?;
    }
    Ok(acc)
}

fn combine_set_plans(
    left: LogicalPlan,
    right: LogicalPlan,
    op: SetOperator,
    quantifier: SetQuantifier,
    pos: Option<Position>,
) -> Result<LogicalPlan, QueryError> {
    let l_schema = left.schema().clone();
    let r_schema = right.schema().clone();
    if l_schema.len() != r_schema.len() {
        return Err(InvalidQuery::new(
            format!(
                "set operation branches have different numbers of columns ({} vs {})",
                l_schema.len(),
                r_schema.len()
            ),
            pos,
        )
        .into());
    }

    let is_set_diff = matches!(
        op,
        SetOperator::Intersect | SetOperator::Except | SetOperator::Minus
    );
    let mut out_fields = Vec::with_capacity(l_schema.len());
    let mut right_cast_fields = Vec::with_capacity(l_schema.len());
    let mut need_left_cast = false;
    let mut need_right_cast = false;
    for (lf, rf) in l_schema.fields().iter().zip(r_schema.fields()) {
        let common_dt = common_set_type(&lf.data_type, &rf.data_type, pos)?;
        if lf.data_type != common_dt {
            need_left_cast = true;
        }
        if rf.data_type != common_dt {
            need_right_cast = true;
        }
        let out_nullable = if is_set_diff {
            lf.nullable
        } else {
            lf.nullable || rf.nullable
        };
        out_fields.push(Field::new(lf.name.clone(), common_dt.clone(), out_nullable));
        right_cast_fields.push(Field::new(lf.name.clone(), common_dt, rf.nullable));
    }
    let out_schema = Schema::try_new_arc(out_fields)?;

    let left = if need_left_cast {
        cast_plan_columns(left, &out_schema)?
    } else {
        left
    };
    let right = if need_right_cast {
        let right_schema = if is_set_diff {
            Schema::try_new_arc(right_cast_fields)?
        } else {
            Arc::clone(&out_schema)
        };
        cast_plan_columns(right, &right_schema)?
    } else {
        right
    };

    match op {
        SetOperator::Union => {
            let all = match quantifier {
                SetQuantifier::All => true,
                SetQuantifier::None | SetQuantifier::Distinct => false,
                _ => {
                    return Err(InvalidQuery::new("unsupported UNION quantifier", pos).into());
                }
            };
            let can_merge = matches!(
                &left,
                LogicalPlan::Union {
                    all: prev_all,
                    schema: prev_schema,
                    ..
                } if *prev_all == all && *prev_schema == out_schema
            );
            if can_merge && let LogicalPlan::Union { mut inputs, .. } = left {
                inputs.push(right);
                return Ok(LogicalPlan::Union {
                    inputs,
                    all,
                    schema: out_schema,
                });
            }
            Ok(LogicalPlan::Union {
                inputs: vec![left, right],
                all,
                schema: out_schema,
            })
        }
        SetOperator::Intersect | SetOperator::Except | SetOperator::Minus => {
            if matches!(quantifier, SetQuantifier::All) {
                return Err(InvalidQuery::new(
                    "INTERSECT ALL and EXCEPT ALL are not supported",
                    pos,
                )
                .into());
            }
            let kind = if matches!(op, SetOperator::Intersect) {
                LogicalJoinKind::Semi
            } else {
                LogicalJoinKind::Anti
            };
            let distinct_left = LogicalPlan::Distinct {
                input: Box::new(left),
            };
            let left_keys: Vec<Expr> = out_schema
                .fields()
                .iter()
                .enumerate()
                .map(|(index, f)| Expr::Column {
                    index,
                    name: f.name.clone(),
                    data_type: f.data_type.clone(),
                })
                .collect();
            let right_keys: Vec<Expr> = right
                .schema()
                .fields()
                .iter()
                .enumerate()
                .map(|(index, f)| Expr::Column {
                    index,
                    name: f.name.clone(),
                    data_type: f.data_type.clone(),
                })
                .collect();
            Ok(LogicalPlan::Join {
                left: Box::new(distinct_left),
                right: Box::new(right),
                kind,
                left_keys,
                right_keys,
                null_equals_null: true,
                residual: None,
                schema: out_schema,
            })
        }
    }
}

fn common_set_type(
    a: &DataType,
    b: &DataType,
    pos: Option<Position>,
) -> Result<DataType, InvalidQuery> {
    if a == b {
        return Ok(a.clone());
    }
    let dummy_a = Expr::Literal(ScalarValue::Null(a.clone()));
    let dummy_b = Expr::Literal(ScalarValue::Null(b.clone()));
    coerce_common_many(&[&dummy_a, &dummy_b], pos)
}

fn cast_plan_columns(
    plan: LogicalPlan,
    target_schema: &Arc<Schema>,
) -> Result<LogicalPlan, QueryError> {
    let in_schema = plan.schema();
    let mut exprs = Vec::with_capacity(in_schema.len());
    for (idx, (in_f, tgt_f)) in in_schema
        .fields()
        .iter()
        .zip(target_schema.fields())
        .enumerate()
    {
        let col_expr = Expr::Column {
            index: idx,
            name: in_f.name.clone(),
            data_type: in_f.data_type.clone(),
        };
        exprs.push(coerce_expr(col_expr, &tgt_f.data_type, None)?);
    }
    Ok(LogicalPlan::Project {
        input: Box::new(plan),
        exprs,
        schema: Arc::clone(target_schema),
    })
}

fn bind_values(values: &Values, ctx: &BindCtx<'_>) -> Result<LogicalPlan, QueryError> {
    let pos = at(values);
    if values.rows.is_empty() {
        return Err(InvalidQuery::new("VALUES requires at least one row", pos).into());
    }
    let n_cols = values.rows[0].len();
    if n_cols == 0 {
        return Err(InvalidQuery::new("VALUES rows cannot be empty", pos).into());
    }
    let empty_scope = Scope::default();
    let mut bound_rows = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        if row.len() != n_cols {
            return Err(InvalidQuery::new("VALUES lists must all be the same length", pos).into());
        }
        let mut bound_row = Vec::with_capacity(n_cols);
        for cell in row.iter() {
            bound_row.push(bind_sql_expr(cell, &empty_scope, ctx, None)?);
        }
        bound_rows.push(bound_row);
    }

    let mut fields = Vec::with_capacity(n_cols);
    let mut col_types = Vec::with_capacity(n_cols);
    for c in 0..n_cols {
        let refs: Vec<&Expr> = bound_rows.iter().map(|r| &r[c]).collect();
        let common_dt = coerce_common_many(&refs, pos)?;
        let nullable = bound_rows
            .iter()
            .any(|r| expr_nullable(&r[c], &empty_scope));
        fields.push(Field::new(
            format!("column{}", c + 1),
            common_dt.clone(),
            nullable,
        ));
        col_types.push(common_dt);
    }

    let mut coerced_rows = Vec::with_capacity(bound_rows.len());
    for row in bound_rows {
        let mut new_row = Vec::with_capacity(n_cols);
        for (cell, dt) in row.into_iter().zip(&col_types) {
            new_row.push(coerce_expr(cell, dt, pos)?);
        }
        coerced_rows.push(new_row);
    }

    let schema = Schema::try_new_arc(fields)?;
    Ok(LogicalPlan::Values {
        schema,
        rows: coerced_rows,
    })
}

fn apply_column_aliases(
    plan: LogicalPlan,
    aliases: &[TableAliasColumnDef],
) -> Result<LogicalPlan, QueryError> {
    let in_schema = plan.schema();
    if aliases.len() > in_schema.len() {
        return Err(InvalidQuery::new(
            format!(
                "column alias list has {} names, but query returns {} columns",
                aliases.len(),
                in_schema.len()
            ),
            at_ident(&aliases[0].name),
        )
        .into());
    }
    let mut fields = Vec::with_capacity(in_schema.len());
    let mut exprs = Vec::with_capacity(in_schema.len());
    for (idx, field) in in_schema.fields().iter().enumerate() {
        let name = if let Some(alias) = aliases.get(idx) {
            ident_canonical(&alias.name)
        } else {
            field.name.clone()
        };
        fields.push(Field::new(name, field.data_type.clone(), field.nullable));
        exprs.push(Expr::Column {
            index: idx,
            name: field.name.clone(),
            data_type: field.data_type.clone(),
        });
    }
    let schema = Schema::try_new_arc(fields)?;
    Ok(LogicalPlan::Project {
        input: Box::new(plan),
        exprs,
        schema,
    })
}

// ---------------------------------------------------------------------------
// Base Views & FROM / Joins / UNNEST
// ---------------------------------------------------------------------------

fn plan_node_count(plan: &LogicalPlan) -> usize {
    let mut count = 0usize;
    let mut stack = vec![plan];
    while let Some(cur) = stack.pop() {
        count += 1;
        match cur {
            LogicalPlan::Scan { .. }
            | LogicalPlan::Values { .. }
            | LogicalPlan::EmptyRow { .. } => {}
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Window { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::TopK { input, .. }
            | LogicalPlan::Distinct { input }
            | LogicalPlan::Unnest { input, .. } => stack.push(input),
            LogicalPlan::Explain { inner, .. } => stack.push(inner),
            LogicalPlan::Join { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            LogicalPlan::Union { inputs, .. } => {
                for inp in inputs {
                    stack.push(inp);
                }
            }
        }
    }
    count
}

fn expand_table(name: &Ident, ctx: &BindCtx<'_>) -> Result<LogicalPlan, QueryError> {
    let lookup_name = ident_canonical(name);
    if let Some(cte) = ctx.ctes.get(&lookup_name) {
        ctx.record_plan_nodes(cte.node_count, at_ident(name))?;
        return Ok(cte.plan.clone());
    }
    let Some(def) = ctx.catalog.get(&lookup_name) else {
        return Err(
            InvalidQuery::new(format!("unknown table `{}`", name.value), at_ident(name)).into(),
        );
    };
    expand_view_def(def, ctx.catalog)
}

fn expand_view_def(def: &SqlTableDef, catalog: &SqlCatalog) -> Result<LogicalPlan, QueryError> {
    match def.kind {
        SqlViewKind::Batch => Ok(LogicalPlan::Scan {
            table: def.name.clone(),
            kind: SqlViewKind::Batch,
            full_schema: Arc::clone(&def.schema),
            projection: None,
            filters: Vec::new(),
            time_range: TimeRangeHint::default(),
            id_lookups: Vec::new(),
            substrings: Vec::new(),
            map_keys: Vec::new(),
            limit: None,
            schema: Arc::clone(&def.schema),
        }),
        SqlViewKind::Spans => {
            let scan = build_windowed_scan(
                def,
                raw_spans_schema(),
                "start_time",
                catalog.window(),
                catalog.visible_from(),
            )?;
            let raw = raw_spans_schema();
            let mut exprs = Vec::with_capacity(56);
            for (index, field) in raw.fields().iter().enumerate() {
                let col_name = field.name.as_str();
                if col_name == "project_id" {
                    continue;
                }
                let col_expr = Expr::Column {
                    index,
                    name: field.name.clone(),
                    data_type: field.data_type.clone(),
                };
                if matches!(col_name, "trace_id" | "span_id" | "parent_span_id") {
                    exprs.push(hex_encode_expr(col_expr));
                } else {
                    exprs.push(col_expr.clone());
                    if col_name == "duration_ns" {
                        exprs.push(duration_ms_expr(col_expr));
                    }
                }
            }
            Ok(LogicalPlan::Project {
                input: Box::new(scan),
                exprs,
                schema: spans_view_schema(),
            })
        }
        SqlViewKind::Traces => expand_traces_view(def, catalog),
        SqlViewKind::Scores => {
            let scan = build_windowed_scan(
                def,
                raw_scores_schema(),
                "created_at",
                catalog.window(),
                catalog.visible_from(),
            )?;
            let raw = raw_scores_schema();
            let mut exprs = Vec::with_capacity(23);
            for (index, field) in raw.fields().iter().enumerate() {
                let col_name = field.name.as_str();
                if col_name == "project_id" {
                    continue;
                }
                let col_expr = Expr::Column {
                    index,
                    name: field.name.clone(),
                    data_type: field.data_type.clone(),
                };
                if matches!(col_name, "trace_id" | "span_id") {
                    exprs.push(hex_encode_expr(col_expr));
                } else {
                    exprs.push(col_expr);
                }
            }
            Ok(LogicalPlan::Project {
                input: Box::new(scan),
                exprs,
                schema: scores_view_schema(),
            })
        }
        SqlViewKind::Sessions => {
            let traces_def = catalog.get("traces").ok_or_else(|| {
                QueryError::internal("traces view must be registered for sessions view")
            })?;
            let traces_plan = expand_traces_view(traces_def, catalog)?;
            let t_schema = traces_plan.schema().clone();
            let col_of = |name: &str| -> Result<Expr, QueryError> {
                let idx = t_schema
                    .index_of(name)
                    .ok_or_else(|| QueryError::internal(format!("missing traces col {name}")))?;
                let f = &t_schema.fields()[idx];
                Ok(Expr::Column {
                    index: idx,
                    name: f.name.clone(),
                    data_type: f.data_type.clone(),
                })
            };
            let session_col = col_of("session_id")?;
            let start_col = col_of("start_time")?;
            let end_col = col_of("end_time")?;
            let trace_id_col = col_of("trace_id")?;
            let error_col = col_of("error_count")?;
            let tokens_col = col_of("total_tokens")?;
            let cost_col = col_of("cost_total")?;
            let user_col = col_of("user_id")?;
            let tags_col = col_of("tags")?;

            let filtered = LogicalPlan::Filter {
                input: Box::new(traces_plan),
                predicate: Expr::Unary {
                    op: UnaryOp::IsNotNull,
                    expr: Box::new(session_col.clone()),
                    data_type: DataType::Bool,
                },
            };

            let agg_schema = Schema::try_new_arc(vec![
                Field::new("session_id", DataType::Utf8, true),
                Field::new("start_time", DataType::timestamp_utc(), true),
                Field::new("end_time", DataType::timestamp_utc(), true),
                Field::new("trace_count", DataType::I64, false),
                Field::new("error_count", DataType::I64, true),
                Field::new("total_tokens", DataType::I64, true),
                Field::new("cost_total", DataType::cost_decimal(), true),
                Field::new("user_id", DataType::Utf8, true),
                Field::new("tags", DataType::List(Box::new(DataType::Utf8)), false),
            ])?;

            let agg = LogicalPlan::Aggregate {
                input: Box::new(filtered),
                group_by: vec![("session_id".to_owned(), session_col)],
                aggs: vec![
                    AggExpr {
                        func: AggFunc::Min(start_col.clone()),
                        filter: None,
                        output_name: "start_time".to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::Max(end_col),
                        filter: None,
                        output_name: "end_time".to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::CountStar,
                        filter: None,
                        output_name: "trace_count".to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::Sum(error_col),
                        filter: None,
                        output_name: "error_count".to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::Sum(tokens_col),
                        filter: None,
                        output_name: "total_tokens".to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::SumCostDecimal(cost_col),
                        filter: None,
                        output_name: "cost_total".to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::FirstNonNullByOrder {
                            value: user_col,
                            order_by: vec![
                                SortKey {
                                    expr: start_col,
                                    descending: false,
                                    nulls_first: true,
                                },
                                SortKey {
                                    expr: trace_id_col,
                                    descending: false,
                                    nulls_first: true,
                                },
                            ],
                        },
                        filter: None,
                        output_name: "user_id".to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::TagUnion(tags_col),
                        filter: None,
                        output_name: "tags".to_owned(),
                    },
                ],
                schema: Arc::clone(&agg_schema),
            };

            let agg_col = |idx: usize| -> Expr {
                let f = &agg_schema.fields()[idx];
                Expr::Column {
                    index: idx,
                    name: f.name.clone(),
                    data_type: f.data_type.clone(),
                }
            };
            let s_id = agg_col(0);
            let s_start = agg_col(1);
            let s_end = agg_col(2);
            let s_trace_count = agg_col(3);
            let s_error_count = agg_col(4);
            let s_total_tokens = agg_col(5);
            let s_cost_total = agg_col(6);
            let s_user_id = agg_col(7);
            let s_tags = agg_col(8);

            let diff_ns = Expr::Binary {
                left: Box::new(Expr::Cast {
                    expr: Box::new(s_end.clone()),
                    target: DataType::I64,
                    mode: CastMode::Strict,
                }),
                op: BinaryOp::Sub,
                right: Box::new(Expr::Cast {
                    expr: Box::new(s_start.clone()),
                    target: DataType::I64,
                    mode: CastMode::Strict,
                }),
                data_type: DataType::I64,
            };
            let duration_ns = Expr::Call {
                func: ScalarFn::Greatest,
                args: vec![diff_ns, Expr::Literal(ScalarValue::I64(0))],
                data_type: DataType::I64,
            };
            let duration_ms = duration_ms_expr(duration_ns.clone());
            let error_coalesced = Expr::Call {
                func: ScalarFn::Coalesce,
                args: vec![s_error_count, Expr::Literal(ScalarValue::I64(0))],
                data_type: DataType::I64,
            };
            let cost_f64 = Expr::Cast {
                expr: Box::new(s_cost_total),
                target: DataType::F64,
                mode: CastMode::Strict,
            };

            Ok(LogicalPlan::Project {
                input: Box::new(agg),
                exprs: vec![
                    s_id,
                    s_start,
                    s_end,
                    duration_ns,
                    duration_ms,
                    s_trace_count,
                    error_coalesced,
                    s_total_tokens,
                    cost_f64,
                    s_user_id,
                    s_tags,
                ],
                schema: sessions_view_schema(),
            })
        }
    }
}

fn expand_traces_view(def: &SqlTableDef, catalog: &SqlCatalog) -> Result<LogicalPlan, QueryError> {
    let scan = build_windowed_scan(
        def,
        raw_traces_schema(),
        "start_time",
        catalog.window(),
        catalog.visible_from(),
    )?;
    let raw = raw_traces_schema();
    let start_idx = raw
        .index_of("start_time")
        .ok_or_else(|| QueryError::internal("raw_traces missing start_time"))?;
    let start_col = Expr::Column {
        index: start_idx,
        name: "start_time".to_owned(),
        data_type: raw.fields()[start_idx].data_type.clone(),
    };
    let mut exprs = Vec::with_capacity(29);
    for (index, field) in raw.fields().iter().enumerate() {
        let col_name = field.name.as_str();
        if matches!(
            col_name,
            "project_id" | "last_ingest_time" | "partial_count"
        ) {
            continue;
        }
        let col_expr = Expr::Column {
            index,
            name: field.name.clone(),
            data_type: field.data_type.clone(),
        };
        if matches!(col_name, "trace_id" | "root_span_id") {
            exprs.push(hex_encode_expr(col_expr));
        } else {
            exprs.push(col_expr.clone());
            if col_name == "end_time" {
                let dur_ns = Expr::Binary {
                    left: Box::new(Expr::Cast {
                        expr: Box::new(col_expr),
                        target: DataType::I64,
                        mode: CastMode::Strict,
                    }),
                    op: BinaryOp::Sub,
                    right: Box::new(Expr::Cast {
                        expr: Box::new(start_col.clone()),
                        target: DataType::I64,
                        mode: CastMode::Strict,
                    }),
                    data_type: DataType::I64,
                };
                let dur_ms = duration_ms_expr(dur_ns.clone());
                exprs.push(dur_ns);
                exprs.push(dur_ms);
            }
        }
    }
    Ok(LogicalPlan::Project {
        input: Box::new(scan),
        exprs,
        schema: traces_view_schema(),
    })
}

fn build_windowed_scan(
    def: &SqlTableDef,
    full_schema: Arc<Schema>,
    window_col: &str,
    window: Option<TimeWindow>,
    visible_from: Option<i64>,
) -> Result<LogicalPlan, QueryError> {
    let idx = full_schema
        .index_of(window_col)
        .ok_or_else(|| QueryError::internal(format!("missing window column {window_col}")))?;
    let dt = full_schema.fields()[idx].data_type.clone();
    let col_expr = Expr::Column {
        index: idx,
        name: window_col.to_owned(),
        data_type: dt,
    };
    let mut filters = Vec::new();
    if let Some(w) = window {
        filters.push(Expr::Binary {
            left: Box::new(col_expr.clone()),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: w.from(),
                utc: true,
            })),
            data_type: DataType::Bool,
        });
        filters.push(Expr::Binary {
            left: Box::new(col_expr.clone()),
            op: BinaryOp::Lt,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: w.to(),
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }
    if let Some(vf) = visible_from {
        filters.push(Expr::Binary {
            left: Box::new(col_expr),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: vf,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }
    Ok(LogicalPlan::Scan {
        table: def.name.clone(),
        kind: def.kind,
        full_schema: Arc::clone(&full_schema),
        projection: None,
        filters,
        time_range: TimeRangeHint::default(),
        id_lookups: Vec::new(),
        substrings: Vec::new(),
        map_keys: Vec::new(),
        limit: None,
        schema: full_schema,
    })
}

fn hex_encode_expr(col: Expr) -> Expr {
    Expr::Call {
        func: ScalarFn::Encode,
        args: vec![col, Expr::Literal(ScalarValue::Utf8("hex".to_owned()))],
        data_type: DataType::Utf8,
    }
}

fn duration_ms_expr(duration_ns: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(Expr::Cast {
            expr: Box::new(duration_ns),
            target: DataType::F64,
            mode: CastMode::Strict,
        }),
        op: BinaryOp::Div,
        right: Box::new(Expr::Literal(ScalarValue::F64(1_000_000.0))),
        data_type: DataType::F64,
    }
}

fn bind_from(
    from: &[TableWithJoins],
    ctx: &BindCtx<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    if from.is_empty() {
        let empty_schema = Schema::try_new_arc(Vec::new())?;
        return Ok((
            LogicalPlan::EmptyRow {
                schema: empty_schema,
            },
            Scope::default(),
        ));
    }

    let mut iter = from.iter();
    let first = iter
        .next()
        .ok_or_else(|| QueryError::internal("empty from slice"))?;
    let (mut plan, mut scope) = bind_table_with_joins(first, ctx)?;
    for twj in iter {
        if let TableFactor::UNNEST { .. } = &twj.relation {
            let (mut next_plan, mut next_scope) =
                bind_unnest_over(plan, scope, &twj.relation, ctx)?;
            for join in &twj.joins {
                let (joined, joined_scope) = bind_join(next_plan, next_scope, join, ctx)?;
                next_plan = joined;
                next_scope = joined_scope;
            }
            plan = next_plan;
            scope = next_scope;
            continue;
        }
        let (right_plan, right_scope) = bind_table_with_joins(twj, ctx)?;
        let (joined, joined_scope) = cross_join(plan, scope, right_plan, right_scope)?;
        plan = joined;
        scope = joined_scope;
    }
    Ok((plan, scope))
}

fn bind_table_with_joins(
    twj: &TableWithJoins,
    ctx: &BindCtx<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let (mut left_plan, mut left_scope) = bind_table_factor(&twj.relation, ctx)?;
    for join in &twj.joins {
        let (next_plan, next_scope) = bind_join(left_plan, left_scope, join, ctx)?;
        left_plan = next_plan;
        left_scope = next_scope;
    }
    Ok((left_plan, left_scope))
}

fn bind_table_factor(
    factor: &TableFactor,
    ctx: &BindCtx<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let pos = at(factor);
    if ctx.depth > MAX_AST_DEPTH {
        return Err(InvalidQuery::new("the query is nested too deeply", pos).into());
    }
    match factor {
        TableFactor::Table {
            name,
            alias,
            args,
            with_ordinality,
            ..
        } => {
            if args.is_some() {
                return Err(InvalidQuery::new("table functions are not allowed", pos).into());
            }
            if *with_ordinality {
                return Err(InvalidQuery::new("WITH ORDINALITY is not supported", pos).into());
            }
            let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
                return Err(InvalidQuery::new(
                    "use the view name alone, without catalog or schema",
                    pos,
                )
                .into());
            };
            let plan = expand_table(ident, ctx)?;
            let mut scope = Scope::from_schema(plan.schema(), Some(&ident_canonical(ident)));
            let plan = apply_table_alias(plan, &mut scope, alias.as_ref())?;
            Ok((plan, scope))
        }
        TableFactor::Derived {
            lateral,
            subquery,
            alias,
            ..
        } => {
            if *lateral {
                return Err(InvalidQuery::new("LATERAL subqueries are not supported", pos).into());
            }
            let sub_ctx = ctx.with_depth(ctx.depth + 1);
            let plan = bind_query(subquery, &sub_ctx)?;
            let mut scope = Scope::from_schema(plan.schema(), None);
            let plan = apply_table_alias(plan, &mut scope, alias.as_ref())?;
            Ok((plan, scope))
        }
        TableFactor::NestedJoin {
            table_with_joins,
            alias,
        } => {
            let sub_ctx = ctx.with_depth(ctx.depth + 1);
            let (mut plan, mut scope) = bind_table_with_joins(table_with_joins, &sub_ctx)?;
            if let Some(table_alias) = alias {
                if scope.cols.iter().any(|c| c.hidden_in_star) {
                    let in_schema = plan.schema();
                    let mut exprs = Vec::with_capacity(scope.cols.len());
                    let mut fields = Vec::with_capacity(scope.cols.len());
                    let mut new_cols = Vec::with_capacity(scope.cols.len());
                    for mut col in scope.cols {
                        if col.hidden_in_star {
                            continue;
                        }
                        let f = &in_schema.fields()[col.index];
                        exprs.push(Expr::Column {
                            index: col.index,
                            name: f.name.clone(),
                            data_type: f.data_type.clone(),
                        });
                        fields.push(f.clone());
                        col.index = new_cols.len();
                        new_cols.push(col);
                    }
                    let schema = Schema::try_new_arc(fields)?;
                    plan = LogicalPlan::Project {
                        input: Box::new(plan),
                        exprs,
                        schema,
                    };
                    scope = Scope { cols: new_cols };
                }
                plan = apply_table_alias(plan, &mut scope, Some(table_alias))?;
            }
            Ok((plan, scope))
        }
        TableFactor::UNNEST { .. } => {
            let empty_schema = Schema::try_new_arc(Vec::new())?;
            let empty = LogicalPlan::EmptyRow {
                schema: empty_schema,
            };
            bind_unnest_over(empty, Scope::default(), factor, ctx)
        }
        _ => Err(InvalidQuery::new("unsupported table factor in FROM", pos).into()),
    }
}

fn apply_table_alias(
    plan: LogicalPlan,
    scope: &mut Scope,
    alias: Option<&TableAlias>,
) -> Result<LogicalPlan, QueryError> {
    let Some(table_alias) = alias else {
        return Ok(plan);
    };
    let qualifier = ident_canonical(&table_alias.name);
    scope.set_qualifier(Some(&qualifier));
    if table_alias.columns.is_empty() {
        return Ok(plan);
    }
    scope.rename_prefix(&table_alias.columns)?;
    apply_column_aliases(plan, &table_alias.columns)
}

fn bind_unnest_over(
    left_plan: LogicalPlan,
    mut left_scope: Scope,
    factor: &TableFactor,
    ctx: &BindCtx<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let pos = at(factor);
    let TableFactor::UNNEST {
        alias,
        array_exprs,
        with_offset,
        with_ordinality,
        ..
    } = factor
    else {
        return Err(QueryError::internal("expected TableFactor::UNNEST"));
    };
    if *with_offset || *with_ordinality {
        return Err(InvalidQuery::new(
            "UNNEST WITH OFFSET and WITH ORDINALITY are not supported",
            pos,
        )
        .into());
    }
    if array_exprs.is_empty() {
        return Err(InvalidQuery::new("UNNEST requires at least one list expression", pos).into());
    }
    if let Some(ta) = alias
        && !ta.columns.is_empty()
        && ta.columns.len() != array_exprs.len()
    {
        return Err(InvalidQuery::new(
            format!(
                "UNNEST alias specifies {} columns, expected {}",
                ta.columns.len(),
                array_exprs.len()
            ),
            pos,
        )
        .into());
    }

    let left_len = left_plan.schema().len();
    let mut proj_exprs: Vec<Expr> = left_plan
        .schema()
        .fields()
        .iter()
        .enumerate()
        .map(|(index, f)| Expr::Column {
            index,
            name: f.name.clone(),
            data_type: f.data_type.clone(),
        })
        .collect();
    let mut proj_fields = left_plan.schema().fields().to_vec();

    let mut unnest_col_indices = Vec::with_capacity(array_exprs.len());
    let mut out_fields = left_plan.schema().fields().to_vec();
    let qualifier = alias.as_ref().map(|a| ident_canonical(&a.name));
    let mut new_scope_cols = Vec::with_capacity(array_exprs.len());

    for (i, expr_ast) in array_exprs.iter().enumerate() {
        let bound = bind_sql_expr(expr_ast, &left_scope, ctx, None)?;
        let DataType::List(inner_dt) = bound.data_type() else {
            return Err(InvalidQuery::new(
                format!(
                    "UNNEST requires a list expression, got `{}`",
                    bound.data_type().display_name()
                ),
                at(expr_ast),
            )
            .into());
        };
        let col_name = if let Some(ta) = alias {
            if let Some(c) = ta.columns.get(i) {
                ident_canonical(&c.name)
            } else if array_exprs.len() == 1 {
                ident_canonical(&ta.name)
            } else {
                format!("unnest_{i}")
            }
        } else if array_exprs.len() == 1 {
            "unnest".to_owned()
        } else {
            format!("unnest_{i}")
        };

        let col_idx = left_len + i;
        proj_fields.push(Field::new(col_name.clone(), bound.data_type(), true));
        proj_exprs.push(bound);
        unnest_col_indices.push(col_idx);

        out_fields.push(Field::new(col_name.clone(), (*inner_dt).clone(), true));
        new_scope_cols.push(ScopeCol {
            qualifier: qualifier.clone(),
            name: col_name,
            data_type: *inner_dt,
            nullable: true,
            index: col_idx,
            hidden_in_star: false,
        });
    }
    left_scope.cols.extend(new_scope_cols);

    let proj_schema = Schema::try_new_arc(proj_fields)?;
    let projected = LogicalPlan::Project {
        input: Box::new(left_plan),
        exprs: proj_exprs,
        schema: proj_schema,
    };
    let out_schema = Schema::try_new_arc(out_fields)?;
    let unnested = LogicalPlan::Unnest {
        input: Box::new(projected),
        columns: unnest_col_indices,
        preserve_nulls: false,
        schema: out_schema,
    };
    Ok((unnested, left_scope))
}

fn cross_join(
    left_plan: LogicalPlan,
    left_scope: Scope,
    right_plan: LogicalPlan,
    right_scope: Scope,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let left_len = left_scope.len();
    let schema = join_output_schema(JoinType::Cross, left_plan.schema(), right_plan.schema());
    let mut combined = left_scope;
    for mut c in right_scope.cols {
        c.index += left_len;
        combined.cols.push(c);
    }
    let plan = LogicalPlan::Join {
        left: Box::new(left_plan),
        right: Box::new(right_plan),
        kind: LogicalJoinKind::Cross,
        left_keys: Vec::new(),
        right_keys: Vec::new(),
        null_equals_null: false,
        residual: None,
        schema,
    };
    Ok((plan, combined))
}

fn bind_join(
    left_plan: LogicalPlan,
    left_scope: Scope,
    join: &Join,
    ctx: &BindCtx<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let pos = at(&join.relation);
    if matches!(
        join.join_operator,
        JoinOperator::CrossJoin(JoinConstraint::None)
    ) && matches!(&join.relation, TableFactor::UNNEST { .. })
    {
        return bind_unnest_over(left_plan, left_scope, &join.relation, ctx);
    }

    let (kind, constraint) = match &join.join_operator {
        JoinOperator::Join(c) | JoinOperator::Inner(c) => (LogicalJoinKind::Inner, c),
        JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (LogicalJoinKind::Left, c),
        JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (LogicalJoinKind::Right, c),
        JoinOperator::FullOuter(c) => (LogicalJoinKind::Full, c),
        JoinOperator::CrossJoin(JoinConstraint::None) => {
            let (right_plan, right_scope) = bind_table_factor(&join.relation, ctx)?;
            return cross_join(left_plan, left_scope, right_plan, right_scope);
        }
        JoinOperator::CrossJoin(_) => {
            return Err(InvalidQuery::new("CROSS JOIN does not take ON or USING", pos).into());
        }
        _ => {
            return Err(InvalidQuery::new("unsupported JOIN type", pos).into());
        }
    };

    let (right_plan, right_scope) = bind_table_factor(&join.relation, ctx)?;
    let left_len = left_scope.len();
    let mut combined_scope = left_scope.clone();
    for mut c in right_scope.cols.iter().cloned() {
        c.index += left_len;
        combined_scope.cols.push(c);
    }

    let exec_join_type = match kind {
        LogicalJoinKind::Inner => JoinType::Inner,
        LogicalJoinKind::Left => JoinType::Left,
        LogicalJoinKind::Right => JoinType::Right,
        LogicalJoinKind::Full => JoinType::Full,
        LogicalJoinKind::Cross => JoinType::Cross,
        _ => return Err(QueryError::internal("unexpected join kind")),
    };

    match constraint {
        JoinConstraint::On(on_ast) => {
            let bound_on = bind_bool_sql_expr(on_ast, &combined_scope, ctx)?;
            let mut left_keys = Vec::new();
            let mut right_keys = Vec::new();
            let mut residuals = Vec::new();
            split_on_conjuncts(
                bound_on,
                left_len,
                &mut left_keys,
                &mut right_keys,
                &mut residuals,
            )?;

            let residual = combine_conjuncts(residuals);
            if matches!(kind, LogicalJoinKind::Left | LogicalJoinKind::Right) {
                combined_scope.widen_nullability(
                    if kind == LogicalJoinKind::Left {
                        left_len
                    } else {
                        0
                    },
                    if kind == LogicalJoinKind::Left {
                        combined_scope.len()
                    } else {
                        left_len
                    },
                );
            } else if kind == LogicalJoinKind::Full {
                combined_scope.widen_nullability(0, combined_scope.len());
            }

            if left_keys.is_empty() {
                if kind == LogicalJoinKind::Inner {
                    let cross_schema = join_output_schema(
                        JoinType::Cross,
                        left_plan.schema(),
                        right_plan.schema(),
                    );
                    let cross = LogicalPlan::Join {
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        kind: LogicalJoinKind::Cross,
                        left_keys: Vec::new(),
                        right_keys: Vec::new(),
                        null_equals_null: false,
                        residual: None,
                        schema: cross_schema,
                    };
                    let out = match residual {
                        Some(predicate) => LogicalPlan::Filter {
                            input: Box::new(cross),
                            predicate,
                        },
                        None => cross,
                    };
                    return Ok((out, combined_scope));
                }
                return Err(InvalidQuery::new(
                    "OUTER JOIN requires at least one equality predicate in ON",
                    pos,
                )
                .into());
            }

            let schema =
                join_output_schema(exec_join_type, left_plan.schema(), right_plan.schema());
            let plan = LogicalPlan::Join {
                left: Box::new(left_plan),
                right: Box::new(right_plan),
                kind,
                left_keys,
                right_keys,
                null_equals_null: false,
                residual,
                schema,
            };
            Ok((plan, combined_scope))
        }
        JoinConstraint::Using(cols) => {
            if cols.is_empty() {
                return Err(InvalidQuery::new("USING requires at least one column", pos).into());
            }
            let mut left_keys = Vec::with_capacity(cols.len());
            let mut right_keys = Vec::with_capacity(cols.len());
            let mut using_pairs = Vec::with_capacity(cols.len());

            for obj in cols {
                let [ObjectNamePart::Identifier(ident)] = obj.0.as_slice() else {
                    return Err(InvalidQuery::new(
                        "USING columns must be unqualified identifiers",
                        at(obj),
                    )
                    .into());
                };
                let Some((l_idx, l_col)) = left_scope.resolve_local(std::slice::from_ref(ident))?
                else {
                    return Err(InvalidQuery::new(
                        format!(
                            "column `{}` specified in USING does not exist in left table",
                            ident_display(ident)
                        ),
                        at_ident(ident),
                    )
                    .into());
                };
                let Some((r_idx, r_col)) =
                    right_scope.resolve_local(std::slice::from_ref(ident))?
                else {
                    return Err(InvalidQuery::new(
                        format!(
                            "column `{}` specified in USING does not exist in right table",
                            ident_display(ident)
                        ),
                        at_ident(ident),
                    )
                    .into());
                };
                let l_expr = Expr::Column {
                    index: l_col.index,
                    name: l_col.name.clone(),
                    data_type: l_col.data_type.clone(),
                };
                let r_expr = Expr::Column {
                    index: r_col.index,
                    name: r_col.name.clone(),
                    data_type: r_col.data_type.clone(),
                };
                let common = coerce_comparison_many(&[&l_expr, &r_expr], at_ident(ident))?;
                left_keys.push(coerce_expr(l_expr, &common, at_ident(ident))?);
                right_keys.push(coerce_expr(r_expr, &common, at_ident(ident))?);
                using_pairs.push((l_idx, left_len + r_idx, common));
            }

            let schema =
                join_output_schema(exec_join_type, left_plan.schema(), right_plan.schema());
            if matches!(kind, LogicalJoinKind::Left | LogicalJoinKind::Full) {
                combined_scope.widen_nullability(left_len, combined_scope.len());
            }
            if matches!(kind, LogicalJoinKind::Right | LogicalJoinKind::Full) {
                combined_scope.widen_nullability(0, left_len);
            }

            let join_plan = LogicalPlan::Join {
                left: Box::new(left_plan),
                right: Box::new(right_plan),
                kind,
                left_keys,
                right_keys,
                null_equals_null: false,
                residual: None,
                schema: Arc::clone(&schema),
            };

            match kind {
                LogicalJoinKind::Inner | LogicalJoinKind::Left => {
                    for &(_, r_combined, _) in &using_pairs {
                        combined_scope.cols[r_combined].hidden_in_star = true;
                    }
                    Ok((join_plan, combined_scope))
                }
                LogicalJoinKind::Right => {
                    for &(l_combined, _, _) in &using_pairs {
                        combined_scope.cols[l_combined].hidden_in_star = true;
                    }
                    Ok((join_plan, combined_scope))
                }
                LogicalJoinKind::Full => {
                    // Project merged `coalesce(left.k, right.k) AS k` columns appended so `*` and
                    // unqualified `k` get the coalesced value while `a.k` / `b.k` keep their own.
                    let base_len = schema.len();
                    let mut proj_exprs: Vec<Expr> = schema
                        .fields()
                        .iter()
                        .enumerate()
                        .map(|(index, f)| Expr::Column {
                            index,
                            name: f.name.clone(),
                            data_type: f.data_type.clone(),
                        })
                        .collect();
                    let mut proj_fields = schema.fields().to_vec();
                    for (offset, &(l_idx, r_idx, ref dt)) in using_pairs.iter().enumerate() {
                        let l_f = &schema.fields()[l_idx];
                        let r_f = &schema.fields()[r_idx];
                        let l_c = coerce_expr(
                            Expr::Column {
                                index: l_idx,
                                name: l_f.name.clone(),
                                data_type: l_f.data_type.clone(),
                            },
                            dt,
                            pos,
                        )?;
                        let r_c = coerce_expr(
                            Expr::Column {
                                index: r_idx,
                                name: r_f.name.clone(),
                                data_type: r_f.data_type.clone(),
                            },
                            dt,
                            pos,
                        )?;
                        let merged = Expr::Call {
                            func: ScalarFn::Coalesce,
                            args: vec![l_c, r_c],
                            data_type: dt.clone(),
                        };
                        let name = l_f.name.clone();
                        proj_exprs.push(merged);
                        proj_fields.push(Field::new(name.clone(), dt.clone(), true));
                        combined_scope.cols[l_idx].hidden_in_star = true;
                        combined_scope.cols[r_idx].hidden_in_star = true;
                        combined_scope.cols.push(ScopeCol {
                            qualifier: None,
                            name,
                            data_type: dt.clone(),
                            nullable: true,
                            index: base_len + offset,
                            hidden_in_star: false,
                        });
                    }
                    let proj_schema = Schema::try_new_arc(proj_fields)?;
                    Ok((
                        LogicalPlan::Project {
                            input: Box::new(join_plan),
                            exprs: proj_exprs,
                            schema: proj_schema,
                        },
                        combined_scope,
                    ))
                }
                _ => Ok((join_plan, combined_scope)),
            }
        }
        JoinConstraint::Natural => {
            Err(InvalidQuery::new("NATURAL JOIN is not supported; use ON or USING", pos).into())
        }
        JoinConstraint::None => {
            Err(InvalidQuery::new("JOIN requires an ON or USING clause", pos).into())
        }
    }
}

fn split_on_conjuncts(
    expr: Expr,
    left_len: usize,
    left_keys: &mut Vec<Expr>,
    right_keys: &mut Vec<Expr>,
    residuals: &mut Vec<Expr>,
) -> Result<(), QueryError> {
    let mut stack = vec![expr];
    while let Some(cur) = stack.pop() {
        match cur {
            Expr::Binary {
                left,
                op: BinaryOp::And,
                right,
                ..
            } => {
                stack.push(*right);
                stack.push(*left);
            }
            Expr::Binary {
                left,
                op: BinaryOp::Eq,
                right,
                data_type,
            } => {
                let l_side = side_of(&left, left_len);
                let r_side = side_of(&right, left_len);
                match (l_side, r_side) {
                    (Side::LeftOnly, Side::RightOnly) => {
                        let shifted_right = shift_right_indices(&right, left_len)?;
                        let common = coerce_comparison_many(&[&left, &shifted_right], None)?;
                        left_keys.push(coerce_expr(*left, &common, None)?);
                        right_keys.push(coerce_expr(shifted_right, &common, None)?);
                    }
                    (Side::RightOnly, Side::LeftOnly) => {
                        let shifted_left = shift_right_indices(&left, left_len)?;
                        let common = coerce_comparison_many(&[&right, &shifted_left], None)?;
                        left_keys.push(coerce_expr(*right, &common, None)?);
                        right_keys.push(coerce_expr(shifted_left, &common, None)?);
                    }
                    _ => residuals.push(Expr::Binary {
                        left,
                        op: BinaryOp::Eq,
                        right,
                        data_type,
                    }),
                }
            }
            other => residuals.push(other),
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Const,
    LeftOnly,
    RightOnly,
    Both,
}

fn side_of(expr: &Expr, left_len: usize) -> Side {
    let mut refs = BTreeSet::new();
    expr.referenced_columns(&mut refs);
    if refs.is_empty() {
        return Side::Const;
    }
    let has_left = refs.iter().any(|&i| i < left_len);
    let has_right = refs.iter().any(|&i| i >= left_len);
    match (has_left, has_right) {
        (true, false) => Side::LeftOnly,
        (false, true) => Side::RightOnly,
        (true, true) => Side::Both,
        (false, false) => Side::Const,
    }
}

fn shift_right_indices(expr: &Expr, left_len: usize) -> Result<Expr, QueryError> {
    let mut refs = BTreeSet::new();
    expr.referenced_columns(&mut refs);
    let max_idx = refs.iter().copied().max().unwrap_or(left_len);
    let mut map = vec![None; max_idx + 1];
    for idx in refs {
        map[idx] = idx.checked_sub(left_len);
    }
    expr.remap_columns(&map)
}

fn combine_conjuncts(exprs: Vec<Expr>) -> Option<Expr> {
    let mut iter = exprs.into_iter();
    let first = iter.next()?;
    Some(iter.fold(first, |acc, e| Expr::Binary {
        left: Box::new(acc),
        op: BinaryOp::And,
        right: Box::new(e),
        data_type: DataType::Bool,
    }))
}

// ---------------------------------------------------------------------------
// WHERE / HAVING with Non-Correlated Subqueries
// ---------------------------------------------------------------------------

fn apply_filter_with_subqueries(
    mut plan: LogicalPlan,
    predicate_ast: &SqlExpr,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    mut bind_plain: impl FnMut(&SqlExpr, &Scope, &BindCtx<'_>) -> Result<Expr, QueryError>,
) -> Result<LogicalPlan, QueryError> {
    let mut conjuncts = Vec::new();
    let mut stack = vec![predicate_ast];
    while let Some(cur) = stack.pop() {
        let mut node = cur;
        while let SqlExpr::Nested(inner) = node {
            node = inner;
        }
        if let SqlExpr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } = node
        {
            stack.push(right);
            stack.push(left);
        } else {
            conjuncts.push(node);
        }
    }

    let mut plain_exprs = Vec::new();
    let sub_ctx = ctx.child_with_outer(scope);
    for conj in conjuncts {
        let (node, outer_negated) = unwrap_not_subquery(conj);
        match node {
            SqlExpr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                if !plain_exprs.is_empty()
                    && let Some(pred) = combine_conjuncts(std::mem::take(&mut plain_exprs))
                {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: pred,
                    };
                }
                let effective_neg = *negated ^ outer_negated;
                let mut right_plan = bind_query(subquery, &sub_ctx)?;
                let r_schema = right_plan.schema().clone();
                if r_schema.len() != 1 {
                    return Err(InvalidQuery::new(
                        format!(
                            "IN subquery must return exactly 1 column, got {}",
                            r_schema.len()
                        ),
                        at(conj),
                    )
                    .into());
                }
                let r_col = Expr::Column {
                    index: 0,
                    name: r_schema.fields()[0].name.clone(),
                    data_type: r_schema.fields()[0].data_type.clone(),
                };
                let left_expr = if let Some((ph_name, ph_pos)) = unwrap_placeholder(expr) {
                    let (_, decl_dt, _) = lookup_param_info(ctx.params, ph_name, ph_pos)?;
                    if needs_usage_coercion(&decl_dt, &r_col.data_type()) {
                        Expr::Literal(coerce_placeholder_to(
                            ctx.params,
                            ph_name,
                            &r_col.data_type(),
                            ph_pos,
                        )?)
                    } else {
                        bind_plain(expr, scope, ctx)?
                    }
                } else {
                    bind_plain(expr, scope, ctx)?
                };
                let common = coerce_comparison_many(&[&left_expr, &r_col], at(conj))?;
                let left_key = coerce_expr(left_expr, &common, at(conj))?;
                let right_key = if r_col.data_type() != common {
                    let cast_expr = coerce_expr(r_col, &common, at(conj))?;
                    let new_schema = Schema::try_new_arc(vec![Field::new(
                        r_schema.fields()[0].name.clone(),
                        common.clone(),
                        r_schema.fields()[0].nullable,
                    )])?;
                    right_plan = LogicalPlan::Project {
                        input: Box::new(right_plan),
                        exprs: vec![cast_expr],
                        schema: new_schema,
                    };
                    Expr::Column {
                        index: 0,
                        name: r_schema.fields()[0].name.clone(),
                        data_type: common,
                    }
                } else {
                    r_col
                };
                let schema = Arc::clone(plan.schema());
                plan = LogicalPlan::Join {
                    left: Box::new(plan),
                    right: Box::new(right_plan),
                    kind: if effective_neg {
                        LogicalJoinKind::AntiNullAware
                    } else {
                        LogicalJoinKind::Semi
                    },
                    left_keys: vec![left_key],
                    right_keys: vec![right_key],
                    null_equals_null: false,
                    residual: None,
                    schema,
                };
            }
            SqlExpr::Exists { subquery, negated } => {
                if !plain_exprs.is_empty()
                    && let Some(pred) = combine_conjuncts(std::mem::take(&mut plain_exprs))
                {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: pred,
                    };
                }
                let effective_neg = *negated ^ outer_negated;
                let sub_plan = bind_query(subquery, &sub_ctx)?;
                let limited = LogicalPlan::Limit {
                    input: Box::new(sub_plan),
                    skip: 0,
                    fetch: Some(1),
                };
                let schema = Arc::clone(plan.schema());
                let true_lit = Expr::Literal(ScalarValue::Bool(true));
                plan = LogicalPlan::Join {
                    left: Box::new(plan),
                    right: Box::new(limited),
                    kind: if effective_neg {
                        LogicalJoinKind::Anti
                    } else {
                        LogicalJoinKind::Semi
                    },
                    left_keys: vec![true_lit.clone()],
                    right_keys: vec![true_lit],
                    null_equals_null: false,
                    residual: None,
                    schema,
                };
            }
            _ => {
                if contains_subquery(conj) {
                    return Err(InvalidQuery::new(
                        "subqueries are only supported as top-level AND conjuncts with IN, NOT IN, EXISTS, or NOT EXISTS in WHERE/HAVING",
                        at(conj),
                    )
                    .into());
                }
                let bound = if let Some((ph_name, ph_pos)) = unwrap_placeholder(conj) {
                    Expr::Literal(coerce_placeholder_to(
                        ctx.params,
                        ph_name,
                        &DataType::Bool,
                        ph_pos,
                    )?)
                } else {
                    bind_plain(conj, scope, ctx)?
                };
                let bool_expr = expect_bool_or_null(bound, at(conj))?;
                plain_exprs.push(bool_expr);
            }
        }
    }

    if let Some(pred) = combine_conjuncts(plain_exprs) {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: pred,
        };
    }
    Ok(plan)
}

fn unwrap_not_subquery(mut expr: &SqlExpr) -> (&SqlExpr, bool) {
    let mut neg = false;
    loop {
        match expr {
            SqlExpr::Nested(inner) => expr = inner,
            SqlExpr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr: inner,
            } if is_direct_subquery(inner) => {
                neg = !neg;
                expr = inner;
            }
            _ => return (expr, neg),
        }
    }
}

fn is_direct_subquery(mut expr: &SqlExpr) -> bool {
    loop {
        match expr {
            SqlExpr::Nested(inner) => expr = inner,
            SqlExpr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr: inner,
            } => expr = inner,
            SqlExpr::InSubquery { .. } | SqlExpr::Exists { .. } => return true,
            _ => return false,
        }
    }
}

fn contains_subquery(ast: &SqlExpr) -> bool {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        match node {
            SqlExpr::InSubquery { .. } | SqlExpr::Exists { .. } | SqlExpr::Subquery(_) => {
                return true;
            }
            SqlExpr::Nested(inner)
            | SqlExpr::UnaryOp { expr: inner, .. }
            | SqlExpr::IsNull(inner)
            | SqlExpr::IsNotNull(inner)
            | SqlExpr::IsUnknown(inner)
            | SqlExpr::IsNotUnknown(inner)
            | SqlExpr::IsTrue(inner)
            | SqlExpr::IsFalse(inner)
            | SqlExpr::IsNotTrue(inner)
            | SqlExpr::IsNotFalse(inner)
            | SqlExpr::Cast { expr: inner, .. }
            | SqlExpr::Ceil { expr: inner, .. }
            | SqlExpr::Floor { expr: inner, .. }
            | SqlExpr::Extract { expr: inner, .. } => stack.push(inner),
            SqlExpr::BinaryOp { left, right, .. }
            | SqlExpr::IsDistinctFrom(left, right)
            | SqlExpr::IsNotDistinctFrom(left, right)
            | SqlExpr::Position {
                expr: left,
                r#in: right,
            } => {
                stack.push(left);
                stack.push(right);
            }
            SqlExpr::Between {
                expr, low, high, ..
            } => {
                stack.push(expr);
                stack.push(low);
                stack.push(high);
            }
            SqlExpr::InList { expr, list, .. } => {
                stack.push(expr);
                stack.extend(list.iter());
            }
            SqlExpr::Like { expr, pattern, .. } | SqlExpr::ILike { expr, pattern, .. } => {
                stack.push(expr);
                stack.push(pattern);
            }
            SqlExpr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(op) = operand {
                    stack.push(op);
                }
                for cw in conditions {
                    stack.push(&cw.condition);
                    stack.push(&cw.result);
                }
                if let Some(el) = else_result {
                    stack.push(el);
                }
            }
            SqlExpr::Array(arr) => stack.extend(arr.elem.iter()),
            SqlExpr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                stack.push(expr);
                if let Some(f) = substring_from {
                    stack.push(f);
                }
                if let Some(l) = substring_for {
                    stack.push(l);
                }
            }
            SqlExpr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                stack.push(expr);
                if let Some(w) = trim_what {
                    stack.push(w);
                }
                if let Some(chars) = trim_characters {
                    stack.extend(chars.iter());
                }
            }
            SqlExpr::CompoundFieldAccess { root, access_chain } => {
                stack.push(root);
                for acc in access_chain {
                    if let sqlparser::ast::AccessExpr::Subscript(
                        sqlparser::ast::Subscript::Index { index },
                    ) = acc
                    {
                        stack.push(index);
                    }
                }
            }
            SqlExpr::JsonAccess { value, path } => {
                stack.push(value);
                for elem in &path.path {
                    if let sqlparser::ast::JsonPathElem::Bracket { key }
                    | sqlparser::ast::JsonPathElem::ColonBracket { key } = elem
                    {
                        stack.push(key);
                    }
                }
            }
            SqlExpr::Function(f) => {
                if let FunctionArguments::List(list) = &f.args {
                    for arg in &list.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            stack.push(e);
                        }
                    }
                }
                if let Some(filter) = &f.filter {
                    stack.push(filter);
                }
            }
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------------------------
// SELECT, GROUP BY, Aggregates, Window Functions, DISTINCT, ORDER BY
// ---------------------------------------------------------------------------

fn bind_select(
    select: &Select,
    ctx: &BindCtx<'_>,
    outer_order_by: Option<&OrderBy>,
) -> Result<(LogicalPlan, PostSelectState), QueryError> {
    let pos = at(select);
    if ctx.depth > MAX_AST_DEPTH {
        return Err(InvalidQuery::new("the query is nested too deeply", pos).into());
    }
    if select.into.is_some() {
        return Err(InvalidQuery::new("SELECT INTO is not allowed", pos).into());
    }
    if !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || select.top.is_some()
        || select.exclude.is_some()
    {
        return Err(InvalidQuery::new("unsupported SELECT clause", pos).into());
    }
    let is_distinct = match &select.distinct {
        None | Some(Distinct::All) => false,
        Some(Distinct::Distinct) => true,
        Some(Distinct::On(_)) => {
            return Err(InvalidQuery::new("SELECT DISTINCT ON is not supported", pos).into());
        }
    };

    let child_ctx = ctx.with_depth(ctx.depth + 1);

    // 1. FROM
    let (mut plan, mut scope) = bind_from(&select.from, &child_ctx)?;

    // 2. WHERE
    if let Some(selection) = &select.selection {
        plan = apply_filter_with_subqueries(plan, selection, &scope, &child_ctx, |e, s, c| {
            bind_plain_where_expr(e, s, c)
        })?;
    }

    // 3. GROUP BY + Aggregates + HAVING
    let gb_exprs = match &select.group_by {
        GroupByExpr::All(_) => {
            return Err(InvalidQuery::new("GROUP BY ALL is not supported", pos).into());
        }
        GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(InvalidQuery::new(
                    "ROLLUP, CUBE, and GROUPING SETS are not supported",
                    pos,
                )
                .into());
            }
            exprs
        }
    };

    let has_agg = !gb_exprs.is_empty()
        || select.having.is_some()
        || select_items_have_aggregate(&select.projection)
        || select.named_window.iter().any(named_window_has_aggregate)
        || outer_order_by.is_some_and(order_by_has_aggregate);

    // Build expanded SelectItem list against `scope`
    let mut expanded_items =
        expand_select_items(&select.projection, &scope, !select.from.is_empty())?;
    let mut order_by_cloned: Option<OrderBy> = if has_agg {
        outer_order_by.map(clone_order_by)
    } else {
        None
    };
    let mut named_windows_cloned: Option<Vec<sqlparser::ast::NamedWindowDefinition>> =
        if has_agg && !select.named_window.is_empty() {
            Some(select.named_window.clone())
        } else {
            None
        };

    if has_agg {
        let mut having_cloned = select.having.clone();
        let (agg_plan, agg_scope) = bind_aggregate_phase(
            plan,
            &scope,
            gb_exprs,
            &mut expanded_items,
            having_cloned.as_mut(),
            order_by_cloned.as_mut(),
            named_windows_cloned.as_deref_mut(),
            &child_ctx,
        )?;
        plan = agg_plan;
        scope = agg_scope;
    }
    let effective_order_by = if has_agg {
        order_by_cloned.as_ref()
    } else {
        outer_order_by
    };

    // 4. Select-list UNNEST(...) (only allowed outside aggregates/window, over post-agg/from scope)
    let mut unnest_state = UnnestCollector::default();
    let mut items_after_unnest = Vec::with_capacity(expanded_items.len());
    for item in expanded_items {
        let mut expr = item.expr;
        extract_select_unnests(&mut expr, &mut unnest_state)?;
        items_after_unnest.push(ExpandedItem {
            name: item.name,
            expr,
            explicit_alias: item.explicit_alias,
            source_hint: item.source_hint,
        });
    }
    if !unnest_state.exprs.is_empty() {
        let (unnested_plan, unnested_scope) = apply_select_unnests(plan, scope, unnest_state, ctx)?;
        plan = unnested_plan;
        scope = unnested_scope;
    }

    // 5. WINDOW functions
    let named_windows = named_windows_cloned
        .as_deref()
        .unwrap_or(&select.named_window);
    let mut win_state = WindowCollector::new(named_windows);
    let mut items_after_window = Vec::with_capacity(items_after_unnest.len());
    for item in items_after_unnest {
        let mut expr = item.expr;
        extract_windows_in_expr(&mut expr, &scope, ctx, &mut win_state)?;
        items_after_window.push(ExpandedItem {
            name: item.name,
            expr,
            explicit_alias: item.explicit_alias,
            source_hint: item.source_hint,
        });
    }
    let mut order_exprs_rewritten: Option<Vec<(OrderByExpr, Option<QualHint>)>> = None;
    if let Some(ob) = effective_order_by {
        let OrderByKind::Expressions(exprs) = &ob.kind else {
            return Err(InvalidQuery::new("ORDER BY ALL is not supported", at(ob)).into());
        };
        if ob.interpolate.is_some() {
            return Err(InvalidQuery::new("ORDER BY INTERPOLATE is not supported", at(ob)).into());
        }
        let mut rewritten = Vec::with_capacity(exprs.len());
        for ob_expr in exprs {
            if ob_expr.with_fill.is_some() {
                return Err(
                    InvalidQuery::new("ORDER BY WITH FILL is not supported", at(ob_expr)).into(),
                );
            }
            let mut cloned_ob = OrderByExpr {
                expr: clone_sql_expr(&ob_expr.expr),
                options: ob_expr.options,
                with_fill: ob_expr.with_fill.clone(),
            };
            let qual_hint = extract_qual_hint(&cloned_ob.expr);
            extract_windows_in_expr(&mut cloned_ob.expr, &scope, ctx, &mut win_state)?;
            rewritten.push((cloned_ob, qual_hint));
        }
        order_exprs_rewritten = Some(rewritten);
    }

    if !win_state.specs.is_empty() {
        let (win_plan, win_scope) = apply_window_groups(plan, scope, win_state)?;
        plan = win_plan;
        scope = win_scope;
    }

    // 6. Bind SELECT projection expressions
    let mut select_bound: Vec<BoundSelectItem> = Vec::with_capacity(items_after_window.len());
    let mut select_fields = Vec::with_capacity(items_after_window.len());
    for item in &items_after_window {
        let bound = bind_sql_expr(&item.expr, &scope, ctx, None)?;
        let nullable = expr_nullable(&bound, &scope);
        select_fields.push(Field::new(item.name.clone(), bound.data_type(), nullable));
        select_bound.push(BoundSelectItem {
            name: item.name.clone(),
            expr: bound,
            source_hint: item.source_hint.clone(),
        });
    }
    let select_len = select_bound.len();

    // 7. Handle DISTINCT + ORDER BY interaction
    if let Some(ob_list) = order_exprs_rewritten {
        let select_alias_pairs: Vec<(String, Expr)> = select_bound
            .iter()
            .map(|b| (b.name.clone(), b.expr.clone()))
            .collect();

        if is_distinct {
            // Project -> Distinct -> Sort (ORDER BY must resolve against SELECT outputs)
            let select_schema = Schema::try_new_arc(select_fields)?;
            let select_exprs: Vec<Expr> = select_bound.iter().map(|b| b.expr.clone()).collect();
            let proj = LogicalPlan::Project {
                input: Box::new(plan),
                exprs: select_exprs,
                schema: Arc::clone(&select_schema),
            };
            let distinct_plan = LogicalPlan::Distinct {
                input: Box::new(proj),
            };

            let mut sort_keys = Vec::with_capacity(ob_list.len());
            for (ob_expr, qual_hint) in &ob_list {
                let key_expr = resolve_distinct_order_expr(
                    &ob_expr.expr,
                    qual_hint.as_ref(),
                    &select_bound,
                    &select_alias_pairs,
                    &scope,
                    ctx,
                    &select_schema,
                )?;
                ensure_sortable_order_by(&key_expr.data_type(), at(ob_expr))?;
                let descending = matches!(ob_expr.options.asc, Some(false));
                let nulls_first = ob_expr.options.nulls_first.unwrap_or(descending);
                sort_keys.push(SortKey {
                    expr: key_expr,
                    descending,
                    nulls_first,
                });
            }
            let sorted = LogicalPlan::Sort {
                input: Box::new(distinct_plan),
                keys: sort_keys,
            };
            return Ok((
                sorted,
                PostSelectState {
                    order_already_applied: true,
                    trim_to_first_n: None,
                },
            ));
        }

        // Non-DISTINCT: build Project(select + any extra ORDER BY exprs) -> Sort -> (Trim later after Limit!)
        let mut all_exprs: Vec<Expr> = select_bound.iter().map(|b| b.expr.clone()).collect();
        let mut all_fields = select_fields;
        let mut sort_keys = Vec::with_capacity(ob_list.len());

        for (ob_expr, _) in &ob_list {
            let bound_ob = resolve_select_order_expr(
                &ob_expr.expr,
                &select_bound,
                &select_alias_pairs,
                &scope,
                ctx,
            )?;
            ensure_sortable_order_by(&bound_ob.data_type(), at(ob_expr))?;
            let col_idx = if let Some(existing) = all_exprs.iter().position(|e| e == &bound_ob) {
                existing
            } else {
                let idx = all_exprs.len();
                let nullable = expr_nullable(&bound_ob, &scope);
                all_fields.push(Field::new(
                    format!("__order_{idx}"),
                    bound_ob.data_type(),
                    nullable,
                ));
                all_exprs.push(bound_ob.clone());
                idx
            };
            let f = &all_fields[col_idx];
            let descending = matches!(ob_expr.options.asc, Some(false));
            let nulls_first = ob_expr.options.nulls_first.unwrap_or(descending);
            sort_keys.push(SortKey {
                expr: Expr::Column {
                    index: col_idx,
                    name: f.name.clone(),
                    data_type: f.data_type.clone(),
                },
                descending,
                nulls_first,
            });
        }

        let ext_schema = Schema::try_new_arc(all_fields)?;
        let proj = LogicalPlan::Project {
            input: Box::new(plan),
            exprs: all_exprs,
            schema: ext_schema,
        };
        let sorted = LogicalPlan::Sort {
            input: Box::new(proj),
            keys: sort_keys,
        };
        let trim_to_first_n =
            (all_exprs_len_check(sorted.schema().len(), select_len)).then_some(select_len);
        return Ok((
            sorted,
            PostSelectState {
                order_already_applied: true,
                trim_to_first_n,
            },
        ));
    }

    let select_schema = Schema::try_new_arc(select_fields)?;
    let select_exprs: Vec<Expr> = select_bound.into_iter().map(|b| b.expr).collect();
    let mut out = LogicalPlan::Project {
        input: Box::new(plan),
        exprs: select_exprs,
        schema: select_schema,
    };
    if is_distinct {
        out = LogicalPlan::Distinct {
            input: Box::new(out),
        };
    }
    Ok((
        out,
        PostSelectState {
            order_already_applied: false,
            trim_to_first_n: None,
        },
    ))
}

fn all_exprs_len_check(extended_len: usize, select_len: usize) -> bool {
    extended_len > select_len
}

fn expr_nullable(expr: &Expr, scope: &Scope) -> bool {
    match expr {
        Expr::Column { index, .. } => scope.cols.get(*index).is_none_or(|c| c.nullable),
        Expr::Literal(sv) => matches!(sv, ScalarValue::Null(_)),
        _ => true,
    }
}

fn bind_plain_where_expr(
    ast: &SqlExpr,
    scope: &Scope,
    ctx: &BindCtx<'_>,
) -> Result<Expr, QueryError> {
    reject_agg_or_window_in_clause(ast, "WHERE")?;
    bind_sql_expr(ast, scope, ctx, None)
}

// ---------------------------------------------------------------------------
// Select Item Expansion & Naming
// ---------------------------------------------------------------------------

type QualHint = (Option<String>, String);

struct BoundSelectItem {
    name: String,
    expr: Expr,
    source_hint: Option<QualHint>,
}

struct WindowGroup {
    partition_by: Vec<Expr>,
    order_by: Vec<SortKey>,
    items: Vec<(WindowSpec, Field)>,
}

#[derive(Clone)]
struct ExpandedItem {
    name: String,
    expr: SqlExpr,
    explicit_alias: bool,
    source_hint: Option<(Option<String>, String)>,
}

fn expand_select_items(
    items: &[SelectItem],
    scope: &Scope,
    has_from: bool,
) -> Result<Vec<ExpandedItem>, QueryError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::Wildcard(opts) => {
                reject_wildcard_opts(opts)?;
                if !has_from {
                    return Err(
                        InvalidQuery::new("SELECT * requires a FROM clause", at(opts)).into(),
                    );
                }
                for col in &scope.cols {
                    if col.hidden_in_star {
                        continue;
                    }
                    let expr = match &col.qualifier {
                        Some(q) => SqlExpr::CompoundIdentifier(vec![
                            Ident::with_quote('"', q.clone()),
                            Ident::with_quote('"', col.name.clone()),
                        ]),
                        None => SqlExpr::Identifier(Ident::with_quote('"', col.name.clone())),
                    };
                    out.push(ExpandedItem {
                        name: col.name.clone(),
                        expr,
                        explicit_alias: false,
                        source_hint: Some((col.qualifier.clone(), col.name.clone())),
                    });
                }
            }
            SelectItem::QualifiedWildcard(kind, opts) => {
                reject_wildcard_opts(opts)?;
                let SelectItemQualifiedWildcardKind::ObjectName(obj) = kind else {
                    return Err(InvalidQuery::new(
                        "expression wildcards are not supported",
                        at(opts),
                    )
                    .into());
                };
                let [ObjectNamePart::Identifier(tbl)] = obj.0.as_slice() else {
                    return Err(
                        InvalidQuery::new("qualified wildcard must be `alias.*`", at(obj)).into(),
                    );
                };
                let matched: Vec<&ScopeCol> = scope
                    .cols
                    .iter()
                    .filter(|c| c.qualifier.as_ref().is_some_and(|q| ident_matches(q, tbl)))
                    .collect();
                if matched.is_empty() {
                    return Err(InvalidQuery::new(
                        format!("unknown table `{}`", ident_display(tbl)),
                        at_ident(tbl),
                    )
                    .into());
                }
                for col in matched {
                    let expr = SqlExpr::CompoundIdentifier(vec![
                        tbl.clone(),
                        Ident::with_quote('"', col.name.clone()),
                    ]);
                    out.push(ExpandedItem {
                        name: col.name.clone(),
                        expr,
                        explicit_alias: false,
                        source_hint: Some((col.qualifier.clone(), col.name.clone())),
                    });
                }
            }
            SelectItem::UnnamedExpr(expr) => {
                let name = default_expr_name(expr);
                let source_hint = extract_qual_hint(expr);
                out.push(ExpandedItem {
                    name,
                    expr: clone_sql_expr(expr),
                    explicit_alias: false,
                    source_hint,
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let source_hint = extract_qual_hint(expr);
                out.push(ExpandedItem {
                    name: ident_canonical(alias),
                    expr: clone_sql_expr(expr),
                    explicit_alias: true,
                    source_hint,
                });
            }
            SelectItem::ExprWithAliases { expr, .. } => {
                return Err(InvalidQuery::new(
                    "multi-alias SELECT items are not supported",
                    at(expr),
                )
                .into());
            }
        }
    }
    Ok(out)
}

fn reject_wildcard_opts(
    opts: &sqlparser::ast::WildcardAdditionalOptions,
) -> Result<(), QueryError> {
    if opts.opt_exclude.is_some()
        || opts.opt_except.is_some()
        || opts.opt_rename.is_some()
        || opts.opt_replace.is_some()
        || opts.opt_ilike.is_some()
    {
        return Err(InvalidQuery::new(
            "wildcard modifiers (EXCLUDE/EXCEPT/RENAME/REPLACE/ILIKE) are not supported",
            at(opts),
        )
        .into());
    }
    Ok(())
}

fn clone_order_by(ob: &OrderBy) -> OrderBy {
    let kind = match &ob.kind {
        OrderByKind::Expressions(exprs) => OrderByKind::Expressions(
            exprs
                .iter()
                .map(|e| OrderByExpr {
                    expr: clone_sql_expr(&e.expr),
                    options: e.options,
                    with_fill: e.with_fill.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    };
    OrderBy {
        kind,
        interpolate: ob.interpolate.clone(),
    }
}

fn fmt_sql_expr_for_name(expr: &SqlExpr) -> String {
    if !matches!(expr, SqlExpr::BinaryOp { .. }) {
        return expr.to_string();
    }
    let mut spine = Vec::new();
    let mut cur = expr;
    while let SqlExpr::BinaryOp { left, op, right } = cur {
        spine.push((op, right.as_ref()));
        cur = left.as_ref();
    }
    let mut out = fmt_sql_expr_for_name(cur);
    for (op, right) in spine.into_iter().rev() {
        out.push(' ');
        out.push_str(&op.to_string());
        out.push(' ');
        out.push_str(&fmt_sql_expr_for_name(right));
    }
    out
}

fn default_expr_name(mut expr: &SqlExpr) -> String {
    while let SqlExpr::Nested(inner) = expr {
        expr = inner;
    }
    match expr {
        SqlExpr::Identifier(ident) => ident_canonical(ident),
        SqlExpr::CompoundIdentifier(parts) => parts
            .last()
            .map(ident_canonical)
            .unwrap_or_else(|| expr.to_string()),
        SqlExpr::BinaryOp { .. } => fmt_sql_expr_for_name(expr),
        _ => expr.to_string(),
    }
}

fn extract_qual_hint(mut expr: &SqlExpr) -> Option<(Option<String>, String)> {
    while let SqlExpr::Nested(inner) = expr {
        expr = inner;
    }
    match expr {
        SqlExpr::Identifier(ident) => Some((None, ident_canonical(ident))),
        SqlExpr::CompoundIdentifier(parts) if parts.len() == 2 => {
            Some((Some(ident_canonical(&parts[0])), ident_canonical(&parts[1])))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// GROUP BY & Aggregate Phase
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum AggKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    ApproxPercentileCont,
    PercentileCont,
    BoolOr,
    BoolAnd,
    ArrayAgg,
}

#[derive(Clone, Copy)]
enum WinKind {
    RowNumber,
    Rank,
    DenseRank,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    Sum,
    Count,
    Avg,
    Min,
    Max,
}

fn classify_agg_name(name: &str) -> Option<AggKind> {
    match name {
        "count" => Some(AggKind::Count),
        "sum" => Some(AggKind::Sum),
        "avg" => Some(AggKind::Avg),
        "min" => Some(AggKind::Min),
        "max" => Some(AggKind::Max),
        "approx_percentile_cont" => Some(AggKind::ApproxPercentileCont),
        "percentile_cont" => Some(AggKind::PercentileCont),
        "bool_or" => Some(AggKind::BoolOr),
        "bool_and" => Some(AggKind::BoolAnd),
        "array_agg" => Some(AggKind::ArrayAgg),
        _ => None,
    }
}

fn classify_win_name(name: &str) -> Option<WinKind> {
    match name {
        "row_number" => Some(WinKind::RowNumber),
        "rank" => Some(WinKind::Rank),
        "dense_rank" => Some(WinKind::DenseRank),
        "lag" => Some(WinKind::Lag),
        "lead" => Some(WinKind::Lead),
        "first_value" => Some(WinKind::FirstValue),
        "last_value" => Some(WinKind::LastValue),
        "sum" => Some(WinKind::Sum),
        "count" => Some(WinKind::Count),
        "avg" => Some(WinKind::Avg),
        "min" => Some(WinKind::Min),
        "max" => Some(WinKind::Max),
        _ => None,
    }
}

fn bare_object_name(name: &ObjectName) -> Option<String> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(ident)] => Some(ident.value.to_ascii_lowercase()),
        _ => None,
    }
}

fn select_items_have_aggregate(items: &[SelectItem]) -> bool {
    items.iter().any(|item| match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
            expr_has_aggregate(e)
        }
        _ => false,
    })
}

fn named_window_has_aggregate(def: &sqlparser::ast::NamedWindowDefinition) -> bool {
    match &def.1 {
        sqlparser::ast::NamedWindowExpr::WindowSpec(spec) => {
            spec.partition_by.iter().any(expr_has_aggregate)
                || spec.order_by.iter().any(|ob| expr_has_aggregate(&ob.expr))
        }
        sqlparser::ast::NamedWindowExpr::NamedWindow(_) => false,
    }
}

fn order_by_has_aggregate(ob: &OrderBy) -> bool {
    let OrderByKind::Expressions(exprs) = &ob.kind else {
        return false;
    };
    exprs.iter().any(|e| expr_has_aggregate(&e.expr))
}

fn expr_has_aggregate(ast: &SqlExpr) -> bool {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        if let SqlExpr::Function(f) = node {
            if f.over.is_none()
                && let Some(name) = bare_object_name(&f.name)
                && classify_agg_name(&name).is_some()
            {
                return true;
            }
            if let FunctionArguments::List(list) = &f.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        stack.push(e);
                    }
                }
            }
            if let Some(filter) = &f.filter {
                stack.push(filter);
            }
            if let Some(WindowType::WindowSpec(spec)) = &f.over {
                for pb in &spec.partition_by {
                    stack.push(pb);
                }
                for ob in &spec.order_by {
                    stack.push(&ob.expr);
                }
            }
            continue;
        }
        push_expr_children(node, &mut stack);
    }
    false
}

fn reject_agg_or_window_in_clause(ast: &SqlExpr, clause: &str) -> Result<(), QueryError> {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        if let SqlExpr::Function(f) = node {
            if f.over.is_some() {
                return Err(InvalidQuery::new(
                    format!("window functions are not allowed in {clause}"),
                    at(node),
                )
                .into());
            }
            if let Some(name) = bare_object_name(&f.name) {
                if classify_agg_name(&name).is_some() {
                    return Err(InvalidQuery::new(
                        format!("aggregate functions are not allowed in {clause}"),
                        at(node),
                    )
                    .into());
                }
                if matches!(
                    name.as_str(),
                    "row_number"
                        | "rank"
                        | "dense_rank"
                        | "lag"
                        | "lead"
                        | "first_value"
                        | "last_value"
                ) {
                    return Err(InvalidQuery::new(
                        format!("window functions are not allowed in {clause}"),
                        at(node),
                    )
                    .into());
                }
            }
            if let FunctionArguments::List(list) = &f.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        stack.push(e);
                    }
                }
            }
            continue;
        }
        push_expr_children(node, &mut stack);
    }
    Ok(())
}

fn push_expr_children<'a>(node: &'a SqlExpr, stack: &mut Vec<&'a SqlExpr>) {
    match node {
        SqlExpr::Nested(inner)
        | SqlExpr::UnaryOp { expr: inner, .. }
        | SqlExpr::IsNull(inner)
        | SqlExpr::IsNotNull(inner)
        | SqlExpr::IsUnknown(inner)
        | SqlExpr::IsNotUnknown(inner)
        | SqlExpr::IsTrue(inner)
        | SqlExpr::IsFalse(inner)
        | SqlExpr::IsNotTrue(inner)
        | SqlExpr::IsNotFalse(inner)
        | SqlExpr::Cast { expr: inner, .. }
        | SqlExpr::Ceil { expr: inner, .. }
        | SqlExpr::Floor { expr: inner, .. }
        | SqlExpr::Extract { expr: inner, .. } => stack.push(inner),
        SqlExpr::BinaryOp { left, right, .. }
        | SqlExpr::IsDistinctFrom(left, right)
        | SqlExpr::IsNotDistinctFrom(left, right)
        | SqlExpr::Position {
            expr: left,
            r#in: right,
        } => {
            stack.push(left);
            stack.push(right);
        }
        SqlExpr::Between {
            expr, low, high, ..
        } => {
            stack.push(expr);
            stack.push(low);
            stack.push(high);
        }
        SqlExpr::InList { expr, list, .. } => {
            stack.push(expr);
            stack.extend(list.iter());
        }
        SqlExpr::Like { expr, pattern, .. } | SqlExpr::ILike { expr, pattern, .. } => {
            stack.push(expr);
            stack.push(pattern);
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                stack.push(op);
            }
            for cw in conditions {
                stack.push(&cw.condition);
                stack.push(&cw.result);
            }
            if let Some(el) = else_result {
                stack.push(el);
            }
        }
        SqlExpr::Array(arr) => stack.extend(arr.elem.iter()),
        SqlExpr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            stack.push(expr);
            if let Some(f) = substring_from {
                stack.push(f);
            }
            if let Some(l) = substring_for {
                stack.push(l);
            }
        }
        SqlExpr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            stack.push(expr);
            if let Some(w) = trim_what {
                stack.push(w);
            }
            if let Some(chars) = trim_characters {
                stack.extend(chars.iter());
            }
        }
        SqlExpr::CompoundFieldAccess { root, access_chain } => {
            stack.push(root);
            for acc in access_chain {
                if let sqlparser::ast::AccessExpr::Subscript(sqlparser::ast::Subscript::Index {
                    index,
                }) = acc
                {
                    stack.push(index);
                }
            }
        }
        SqlExpr::JsonAccess { value, path } => {
            stack.push(value);
            for elem in &path.path {
                if let sqlparser::ast::JsonPathElem::Bracket { key }
                | sqlparser::ast::JsonPathElem::ColonBracket { key } = elem
                {
                    stack.push(key);
                }
            }
        }
        _ => {}
    }
}

struct GroupEntry {
    name: String,
    qualifier: Option<String>,
    from_expr: Expr,
    bin_op_count: usize,
    nullable: bool,
    synth_name: String,
}

struct AggCollector {
    aggs: Vec<AggExpr>,
    agg_fields: Vec<Field>,
    synth_names: Vec<String>,
    group_entries: Vec<GroupEntry>,
}

impl AggCollector {
    fn register_agg(
        &mut self,
        func: AggFunc,
        filter: Option<Expr>,
        data_type: DataType,
        nullable: bool,
    ) -> String {
        if let Some(idx) = self
            .aggs
            .iter()
            .position(|a| a.func == func && a.filter == filter)
        {
            return self.synth_names[idx].clone();
        }
        let idx = self.aggs.len();
        let output_name = format!("__agg_{idx}");
        let synth_name = format!("__cetra_agg_{idx}");
        self.aggs.push(AggExpr {
            func,
            filter,
            output_name: output_name.clone(),
        });
        self.agg_fields
            .push(Field::new(output_name, data_type, nullable));
        self.synth_names.push(synth_name.clone());
        synth_name
    }
}

#[expect(clippy::too_many_arguments)]
fn bind_aggregate_phase(
    plan: LogicalPlan,
    from_scope: &Scope,
    gb_exprs: &[SqlExpr],
    expanded_items: &mut [ExpandedItem],
    mut having: Option<&mut SqlExpr>,
    order_by: Option<&mut OrderBy>,
    named_windows: Option<&mut [sqlparser::ast::NamedWindowDefinition]>,
    ctx: &BindCtx<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let mut group_entries = Vec::with_capacity(gb_exprs.len());
    for (g_idx, gb_ast) in gb_exprs.iter().enumerate() {
        let resolved_ast = resolve_group_by_ast(gb_ast, from_scope, expanded_items)?;
        reject_agg_or_window_in_clause(resolved_ast, "GROUP BY")?;
        let bound = bind_sql_expr(resolved_ast, from_scope, ctx, None)?;
        let nullable = expr_nullable(&bound, from_scope);
        let (qualifier, name) = if let Expr::Column { index, .. } = &bound
            && let Some(sc) = from_scope.cols.get(*index)
        {
            (sc.qualifier.clone(), sc.name.clone())
        } else {
            (None, default_expr_name(resolved_ast))
        };
        let bin_op_count = ir_bin_count(&bound);
        group_entries.push(GroupEntry {
            name: if name.is_empty() {
                format!("__group_{g_idx}")
            } else {
                name
            },
            qualifier,
            from_expr: bound,
            bin_op_count,
            nullable,
            synth_name: format!("__cetra_gb_{g_idx}"),
        });
    }

    let mut collector = AggCollector {
        aggs: Vec::new(),
        agg_fields: Vec::new(),
        synth_names: Vec::new(),
        group_entries,
    };

    for item in expanded_items.iter_mut() {
        rewrite_agg_expr(&mut item.expr, from_scope, ctx, &mut collector, None)?;
    }

    if let Some(having_ast) = having.as_deref_mut() {
        rewrite_agg_subquery_expr(having_ast, from_scope, ctx, &mut collector)?;
    }

    if let Some(named) = named_windows {
        for def in named {
            if let sqlparser::ast::NamedWindowExpr::WindowSpec(spec) = &mut def.1 {
                for pb in &mut spec.partition_by {
                    rewrite_agg_expr(pb, from_scope, ctx, &mut collector, None)?;
                }
                for ob in &mut spec.order_by {
                    rewrite_agg_expr(&mut ob.expr, from_scope, ctx, &mut collector, None)?;
                }
            }
        }
    }

    if let Some(ob) = order_by
        && let OrderByKind::Expressions(exprs) = &mut ob.kind
    {
        for ob_expr in exprs {
            if is_select_ordinal(&ob_expr.expr, expanded_items.len()) {
                continue;
            }
            if let Some(ident) = unwrap_bare_ident(&ob_expr.expr)
                && expanded_items
                    .iter()
                    .any(|it| ident_matches(&it.name, ident))
            {
                continue;
            }
            rewrite_agg_expr(
                &mut ob_expr.expr,
                from_scope,
                ctx,
                &mut collector,
                Some(expanded_items),
            )?;
        }
    }

    let mut agg_fields =
        Vec::with_capacity(collector.group_entries.len() + collector.agg_fields.len());
    let mut group_by_pairs = Vec::with_capacity(collector.group_entries.len());
    let mut agg_scope_cols =
        Vec::with_capacity(collector.group_entries.len() + collector.agg_fields.len());

    for (idx, g) in collector.group_entries.iter().enumerate() {
        let dt = g.from_expr.data_type();
        agg_fields.push(Field::new(g.name.clone(), dt.clone(), g.nullable));
        group_by_pairs.push((g.name.clone(), g.from_expr.clone()));
        agg_scope_cols.push(ScopeCol {
            qualifier: g.qualifier.clone(),
            name: g.synth_name.clone(),
            data_type: dt,
            nullable: g.nullable,
            index: idx,
            hidden_in_star: false,
        });
    }

    let gb_len = collector.group_entries.len();
    for (a_idx, (field, synth)) in collector
        .agg_fields
        .iter()
        .zip(collector.synth_names.iter())
        .enumerate()
    {
        agg_fields.push(field.clone());
        agg_scope_cols.push(ScopeCol {
            qualifier: None,
            name: synth.clone(),
            data_type: field.data_type.clone(),
            nullable: field.nullable,
            index: gb_len + a_idx,
            hidden_in_star: false,
        });
    }

    let agg_schema = Schema::try_new_arc(agg_fields)?;
    let mut agg_plan = LogicalPlan::Aggregate {
        input: Box::new(plan),
        group_by: group_by_pairs,
        aggs: collector.aggs,
        schema: agg_schema,
    };
    let agg_scope = Scope {
        cols: agg_scope_cols,
    };

    if let Some(having_ast) = having {
        agg_plan =
            apply_filter_with_subqueries(agg_plan, having_ast, &agg_scope, ctx, |e, s, c| {
                bind_sql_expr(e, s, c, None)
            })?;
    }

    Ok((agg_plan, agg_scope))
}

fn resolve_group_by_ast<'a>(
    gb_ast: &'a SqlExpr,
    from_scope: &Scope,
    expanded_items: &'a [ExpandedItem],
) -> Result<&'a SqlExpr, QueryError> {
    let mut cur = gb_ast;
    while let SqlExpr::Nested(inner) = cur {
        cur = inner;
    }
    if let SqlExpr::Value(vws) = cur
        && let Value::Number(s, false) = &vws.value
        && let Ok(ord) = s.parse::<usize>()
    {
        if ord == 0 || ord > expanded_items.len() {
            return Err(InvalidQuery::new(
                format!("GROUP BY position {ord} is not in select list"),
                at(gb_ast),
            )
            .into());
        }
        return Ok(&expanded_items[ord - 1].expr);
    }
    if let SqlExpr::Identifier(ident) = cur {
        if from_scope
            .resolve_local(std::slice::from_ref(ident))?
            .is_some()
        {
            return Ok(gb_ast);
        }
        let mut matches = expanded_items
            .iter()
            .filter(|it| it.explicit_alias && ident_matches(&it.name, ident));
        if let Some(first) = matches.next() {
            if matches.next().is_some() {
                return Err(InvalidQuery::new(
                    format!("ambiguous column `{}` in GROUP BY", ident_display(ident)),
                    at_ident(ident),
                )
                .into());
            }
            return Ok(&first.expr);
        }
    }
    Ok(gb_ast)
}

fn rewrite_agg_subquery_expr(
    ast: &mut SqlExpr,
    from_scope: &Scope,
    ctx: &BindCtx<'_>,
    collector: &mut AggCollector,
) -> Result<(), QueryError> {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        let mut cur = node;
        while let SqlExpr::Nested(inner) = cur {
            cur = inner;
        }
        if let SqlExpr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } = cur
        {
            stack.push(right);
            stack.push(left);
            continue;
        }
        if let SqlExpr::InSubquery { expr, .. } = cur {
            rewrite_agg_expr(expr, from_scope, ctx, collector, None)?;
            continue;
        }
        if matches!(cur, SqlExpr::Exists { .. }) {
            continue;
        }
        if matches!(
            cur,
            SqlExpr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr,
            } if is_direct_subquery(expr)
        ) {
            if let SqlExpr::UnaryOp { expr, .. } = cur {
                stack.push(expr);
            }
            continue;
        }
        rewrite_agg_expr(cur, from_scope, ctx, collector, None)?;
    }
    Ok(())
}

fn expr_has_window(ast: &SqlExpr) -> bool {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        if let SqlExpr::Function(f) = node {
            if f.over.is_some() {
                return true;
            }
            if let FunctionArguments::List(list) = &f.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        stack.push(e);
                    }
                }
            }
            continue;
        }
        push_expr_children(node, &mut stack);
    }
    false
}

fn ir_bin_count(expr: &Expr) -> usize {
    let mut count = 0usize;
    let mut stack = vec![expr];
    while let Some(cur) = stack.pop() {
        match cur {
            Expr::Binary { left, right, .. } => {
                count += 1;
                stack.push(left);
                stack.push(right);
            }
            Expr::Unary { expr: inner, .. } | Expr::Cast { expr: inner, .. } => {
                stack.push(inner);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                stack.push(expr);
                stack.push(low);
                stack.push(high);
            }
            Expr::InList { expr, list, .. } => {
                stack.push(expr);
                stack.extend(list.iter());
            }
            Expr::Like { expr, pattern, .. } => {
                stack.push(expr);
                stack.push(pattern);
            }
            Expr::Case {
                operand,
                when_then,
                else_expr,
                ..
            } => {
                if let Some(op) = operand {
                    stack.push(op);
                }
                for (w, t) in when_then {
                    stack.push(w);
                    stack.push(t);
                }
                if let Some(el) = else_expr {
                    stack.push(el);
                }
            }
            Expr::MapIndex { map, key } => {
                stack.push(map);
                stack.push(key);
            }
            Expr::MakeArray { elems, .. } => stack.extend(elems.iter()),
            Expr::Call { func, args, .. } => {
                if matches!(func, ScalarFn::JsonGet | ScalarFn::JsonGetText) {
                    count += 1;
                }
                stack.extend(args.iter());
            }
            Expr::Column { .. } | Expr::Literal(_) => {}
        }
    }
    count
}

fn count_sql_bin_ops(ast: &SqlExpr) -> usize {
    let mut count = 0usize;
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        match node {
            SqlExpr::BinaryOp { .. }
            | SqlExpr::IsDistinctFrom(_, _)
            | SqlExpr::IsNotDistinctFrom(_, _) => {
                count += 1;
            }
            SqlExpr::JsonAccess { path, .. } => {
                count += path.path.len();
            }
            SqlExpr::Function(f) => {
                if let Some(name) = bare_object_name(&f.name)
                    && matches!(name.as_str(), "json_get" | "json_get_text")
                {
                    count += 1;
                }
                if let FunctionArguments::List(list) = &f.args {
                    for arg in &list.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            stack.push(e);
                        }
                    }
                }
                continue;
            }
            _ => {}
        }
        push_expr_children(node, &mut stack);
    }
    count
}

fn expr_has_agg_win_or_subquery(ast: &SqlExpr) -> bool {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        match node {
            SqlExpr::InSubquery { .. } | SqlExpr::Exists { .. } | SqlExpr::Subquery(_) => {
                return true;
            }
            SqlExpr::Function(f) => {
                if f.over.is_some() {
                    return true;
                }
                if let Some(name) = bare_object_name(&f.name)
                    && classify_agg_name(&name).is_some()
                {
                    return true;
                }
                if let FunctionArguments::List(list) = &f.args {
                    for arg in &list.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            stack.push(e);
                        }
                    }
                }
                if let Some(filter) = &f.filter {
                    stack.push(filter);
                }
                continue;
            }
            _ => {}
        }
        push_expr_children(node, &mut stack);
    }
    false
}

fn find_matching_binary_group_prefix(
    node: &SqlExpr,
    from_scope: &Scope,
    ctx: &BindCtx<'_>,
    collector: &AggCollector,
) -> Option<(usize, usize, String)> {
    let mut steps: Vec<&SqlExpr> = Vec::new();
    let mut cur = node;
    while let SqlExpr::BinaryOp { left, .. } = cur {
        steps.push(cur);
        cur = left.as_ref();
    }
    if expr_has_agg_win_or_subquery(cur) {
        return None;
    }
    let total_steps = steps.len();
    steps.reverse();
    let mut bin_ops = count_sql_bin_ops(cur);
    let mut best: Option<(usize, usize, String)> = None;
    for (idx, step) in steps.iter().enumerate() {
        let SqlExpr::BinaryOp { right, .. } = step else {
            break;
        };
        if expr_has_agg_win_or_subquery(right) {
            break;
        }
        bin_ops += 1 + count_sql_bin_ops(right);
        if collector
            .group_entries
            .iter()
            .any(|g| g.bin_op_count == bin_ops)
            && let Ok(bound) = bind_sql_expr(step, from_scope, ctx, None)
            && let Some(g) = collector
                .group_entries
                .iter()
                .find(|g| g.from_expr == bound)
        {
            best = Some((total_steps, idx, g.synth_name.clone()));
        }
    }
    best
}

fn rewrite_agg_expr(
    ast: &mut SqlExpr,
    from_scope: &Scope,
    ctx: &BindCtx<'_>,
    collector: &mut AggCollector,
    select_items_for_order: Option<&[ExpandedItem]>,
) -> Result<(), QueryError> {
    let has_compound_group = collector.group_entries.iter().any(|g| g.bin_op_count > 0);
    let mut work: Vec<(&mut SqlExpr, usize)> = vec![(ast, 0)];
    while let Some((node, depth)) = work.pop() {
        if depth > MAX_AST_DEPTH {
            return Err(InvalidQuery::new("the expression is nested too deeply", at(node)).into());
        }

        if matches!(node, SqlExpr::BinaryOp { .. }) {
            let best = if has_compound_group {
                find_matching_binary_group_prefix(node, from_scope, ctx, collector)
            } else {
                None
            };
            let mut cur_mut: &mut SqlExpr = node;
            let mut right_children: Vec<&mut SqlExpr> = Vec::new();
            let mut leaf_opt: Option<&mut SqlExpr> = None;
            for hop in 0usize.. {
                if let Some((total_steps, match_idx, ref synth_name)) = best
                    && hop == total_steps - 1 - match_idx
                {
                    *cur_mut = SqlExpr::Identifier(Ident::with_quote('"', synth_name.clone()));
                    break;
                }
                match cur_mut {
                    SqlExpr::BinaryOp { left, right, .. } => {
                        right_children.push(right.as_mut());
                        cur_mut = left.as_mut();
                    }
                    _ => {
                        leaf_opt = Some(cur_mut);
                        break;
                    }
                }
            }
            for rc in right_children {
                work.push((rc, depth + 1));
            }
            if let Some(leaf) = leaf_opt {
                work.push((leaf, depth));
            }
            continue;
        }

        if !expr_has_aggregate(node) && !expr_has_window(node) {
            if !contains_subquery(node)
                && let Ok(bound_from) = bind_sql_expr(node, from_scope, ctx, None)
                && let Some(g) = collector
                    .group_entries
                    .iter()
                    .find(|g| g.from_expr == bound_from)
            {
                *node = SqlExpr::Identifier(Ident::with_quote('"', g.synth_name.clone()));
                continue;
            }

            // Check bare / qualified identifier that wasn't matched as a GROUP BY expression.
            match node {
                SqlExpr::Identifier(ident) => {
                    if let Some(items) = select_items_for_order
                        && items.iter().any(|it| ident_matches(&it.name, ident))
                        && from_scope
                            .resolve_local(std::slice::from_ref(ident))
                            .ok()
                            .flatten()
                            .is_none()
                    {
                        continue;
                    }
                    if from_scope
                        .resolve_local(std::slice::from_ref(ident))?
                        .is_some()
                    {
                        return Err(InvalidQuery::new(
                            format!(
                                "column `{}` must appear in the GROUP BY clause or be used in an aggregate function",
                                ident_display(ident)
                            ),
                            at_ident(ident),
                        )
                        .into());
                    }
                    continue;
                }
                SqlExpr::CompoundIdentifier(parts) => {
                    if from_scope.resolve_local(parts)?.is_some() {
                        let label = parts
                            .iter()
                            .map(ident_display)
                            .collect::<Vec<_>>()
                            .join(".");
                        return Err(InvalidQuery::new(
                            format!(
                                "column `{label}` must appear in the GROUP BY clause or be used in an aggregate function"
                            ),
                            parts.first().and_then(at_ident),
                        )
                        .into());
                    }
                    continue;
                }
                _ => {}
            }
        }

        // Check if `node` itself is an aggregate function call.
        if let SqlExpr::Function(f) = node
            && f.over.is_none()
            && let Some(name) = bare_object_name(&f.name)
            && let Some(kind) = classify_agg_name(&name)
        {
            let (agg_func, filter, out_dt, nullable) = bind_agg_call(kind, f, from_scope, ctx)?;
            let synth = collector.register_agg(agg_func, filter, out_dt, nullable);
            *node = SqlExpr::Identifier(Ident::with_quote('"', synth));
            continue;
        }

        // Otherwise push children onto `work` in reverse so left-to-right order is preserved.
        match node {
            SqlExpr::Function(f) => {
                if let Some(WindowType::WindowSpec(spec)) = &mut f.over {
                    for ob in spec.order_by.iter_mut().rev() {
                        work.push((&mut ob.expr, depth + 1));
                    }
                    for pb in spec.partition_by.iter_mut().rev() {
                        work.push((pb, depth + 1));
                    }
                }
                if let FunctionArguments::List(list) = &mut f.args {
                    for arg in list.args.iter_mut().rev() {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            work.push((e, depth + 1));
                        }
                    }
                }
            }
            _ => {
                let mut children = Vec::new();
                push_expr_children_mut(node, &mut children);
                for child in children.into_iter().rev() {
                    work.push((child, depth + 1));
                }
            }
        }
    }
    Ok(())
}

fn bind_agg_call(
    kind: AggKind,
    function: &Function,
    from_scope: &Scope,
    ctx: &BindCtx<'_>,
) -> Result<(AggFunc, Option<Expr>, DataType, bool), QueryError> {
    let pos = at(function);
    if function.uses_odbc_syntax || function.null_treatment.is_some() {
        return Err(InvalidQuery::new("unsupported aggregate modifier", pos).into());
    }
    let parameters_empty = match &function.parameters {
        FunctionArguments::None => true,
        FunctionArguments::List(l) => l.args.is_empty(),
        FunctionArguments::Subquery(_) => false,
    };
    if !parameters_empty {
        return Err(InvalidQuery::new("function parameters are not supported", pos).into());
    }
    let FunctionArguments::List(arg_list) = &function.args else {
        return Err(InvalidQuery::new("aggregate functions require parentheses", pos).into());
    };
    if !arg_list.clauses.is_empty() {
        return Err(
            InvalidQuery::new("clauses inside aggregate arguments are not supported", pos).into(),
        );
    }
    let distinct = match arg_list.duplicate_treatment {
        None | Some(sqlparser::ast::DuplicateTreatment::All) => false,
        Some(sqlparser::ast::DuplicateTreatment::Distinct) => true,
    };

    let filter = if let Some(filter_ast) = &function.filter {
        reject_agg_or_window_in_clause(filter_ast, "FILTER")?;
        Some(bind_bool_sql_expr(filter_ast, from_scope, ctx)?)
    } else {
        None
    };

    let mut arg_exprs: Vec<&SqlExpr> = Vec::with_capacity(arg_list.args.len());
    let mut has_wildcard = false;
    for arg in &arg_list.args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => has_wildcard = true,
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                reject_agg_or_window_in_clause(e, "aggregate argument")?;
                arg_exprs.push(e);
            }
            _ => {
                return Err(InvalidQuery::new("unsupported aggregate argument", pos).into());
            }
        }
    }

    match kind {
        AggKind::Count => {
            if !function.within_group.is_empty() {
                return Err(InvalidQuery::new("COUNT does not take WITHIN GROUP", pos).into());
            }
            if has_wildcard {
                if arg_list.args.len() != 1 || distinct {
                    return Err(InvalidQuery::new("COUNT(DISTINCT *) is not supported", pos).into());
                }
                return Ok((AggFunc::CountStar, filter, DataType::I64, false));
            }
            if arg_exprs.len() != 1 {
                return Err(InvalidQuery::new("COUNT() takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_exprs[0], from_scope, ctx, None)?;
            if distinct {
                DataType::List(Box::new(e.data_type()))
                    .validate(cetra_column::MAX_NESTING_DEPTH)
                    .map_err(|err| InvalidQuery::new(err.to_string(), pos))?;
                Ok((AggFunc::CountDistinct(e), filter, DataType::I64, false))
            } else {
                Ok((AggFunc::Count(e), filter, DataType::I64, false))
            }
        }
        AggKind::Sum => {
            if has_wildcard || distinct || !function.within_group.is_empty() || arg_exprs.len() != 1
            {
                return Err(InvalidQuery::new(
                    "SUM() takes 1 non-distinct expression argument",
                    pos,
                )
                .into());
            }
            let e = bind_sql_expr(arg_exprs[0], from_scope, ctx, None)?;
            let out_dt = match e.data_type() {
                DataType::I64 => DataType::I64,
                DataType::F64 => DataType::F64,
                DataType::Decimal128 { scale, .. } => DataType::Decimal128 {
                    precision: 38,
                    scale,
                },
                other => {
                    return Err(InvalidQuery::new(
                        format!(
                            "SUM() requires numeric input, got `{}`",
                            other.display_name()
                        ),
                        pos,
                    )
                    .into());
                }
            };
            Ok((AggFunc::Sum(e), filter, out_dt, true))
        }
        AggKind::Avg => {
            if has_wildcard || distinct || !function.within_group.is_empty() || arg_exprs.len() != 1
            {
                return Err(InvalidQuery::new(
                    "AVG() takes 1 non-distinct expression argument",
                    pos,
                )
                .into());
            }
            let e = bind_sql_expr(arg_exprs[0], from_scope, ctx, None)?;
            let out_dt = match e.data_type() {
                DataType::I64 | DataType::F64 => DataType::F64,
                DataType::Decimal128 { precision, scale } => {
                    let out_p = precision.saturating_add(4).min(38);
                    let out_s = (i16::from(scale) + 4).min(i16::from(out_p)) as i8;
                    DataType::Decimal128 {
                        precision: out_p,
                        scale: out_s,
                    }
                }
                other => {
                    return Err(InvalidQuery::new(
                        format!(
                            "AVG() requires numeric input, got `{}`",
                            other.display_name()
                        ),
                        pos,
                    )
                    .into());
                }
            };
            Ok((AggFunc::Avg(e), filter, out_dt, true))
        }
        AggKind::Min | AggKind::Max => {
            if has_wildcard || !function.within_group.is_empty() || arg_exprs.len() != 1 {
                return Err(InvalidQuery::new("MIN/MAX takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_exprs[0], from_scope, ctx, None)?;
            let dt = e.data_type();
            if !is_sortable_dt(&dt) {
                return Err(InvalidQuery::new(
                    format!("MIN/MAX does not support `{}`", dt.display_name()),
                    pos,
                )
                .into());
            }
            let func = if matches!(kind, AggKind::Min) {
                AggFunc::Min(e)
            } else {
                AggFunc::Max(e)
            };
            Ok((func, filter, dt, true))
        }
        AggKind::ApproxPercentileCont => {
            if has_wildcard || distinct || !function.within_group.is_empty() || arg_exprs.len() != 2
            {
                return Err(InvalidQuery::new(
                    "approx_percentile_cont() takes 2 arguments: (expr, quantile)",
                    pos,
                )
                .into());
            }
            let e = bind_sql_expr(arg_exprs[0], from_scope, ctx, None)?;
            if !matches!(
                e.data_type(),
                DataType::I64 | DataType::F64 | DataType::Decimal128 { .. }
            ) {
                return Err(InvalidQuery::new(
                    "approx_percentile_cont requires numeric input",
                    pos,
                )
                .into());
            }
            let quantile = eval_const_f64_arg(arg_exprs[1], ctx, pos)?;
            if !quantile.is_finite() || !(0.0..=1.0).contains(&quantile) {
                return Err(QueryError::failed(
                    "approx_percentile_cont quantile must be between 0.0 and 1.0",
                ));
            }
            Ok((
                AggFunc::ApproxPercentileCont { expr: e, quantile },
                filter,
                DataType::F64,
                true,
            ))
        }
        AggKind::PercentileCont => {
            if has_wildcard || distinct || arg_exprs.len() != 1 || function.within_group.len() != 1
            {
                return Err(InvalidQuery::new(
                    "percentile_cont(quantile) requires WITHIN GROUP (ORDER BY expr)",
                    pos,
                )
                .into());
            }
            let raw_q = eval_const_f64_arg(arg_exprs[0], ctx, pos)?;
            if !raw_q.is_finite() || !(0.0..=1.0).contains(&raw_q) {
                return Err(QueryError::failed(
                    "percentile_cont quantile must be between 0.0 and 1.0",
                ));
            }
            let ob = &function.within_group[0];
            reject_agg_or_window_in_clause(&ob.expr, "WITHIN GROUP")?;
            let e = bind_sql_expr(&ob.expr, from_scope, ctx, None)?;
            if !matches!(
                e.data_type(),
                DataType::I64 | DataType::F64 | DataType::Decimal128 { .. }
            ) {
                return Err(
                    InvalidQuery::new("percentile_cont requires numeric input", pos).into(),
                );
            }
            let quantile = if matches!(ob.options.asc, Some(false)) {
                1.0 - raw_q
            } else {
                raw_q
            };
            Ok((
                AggFunc::PercentileCont { expr: e, quantile },
                filter,
                DataType::F64,
                true,
            ))
        }
        AggKind::BoolOr | AggKind::BoolAnd => {
            if has_wildcard || distinct || !function.within_group.is_empty() || arg_exprs.len() != 1
            {
                return Err(InvalidQuery::new("BOOL_OR/BOOL_AND takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_exprs[0], from_scope, ctx, None)?;
            let e = expect_bool_or_null(e, pos)?;
            let func = if matches!(kind, AggKind::BoolOr) {
                AggFunc::BoolOr(e)
            } else {
                AggFunc::BoolAnd(e)
            };
            Ok((func, filter, DataType::Bool, true))
        }
        AggKind::ArrayAgg => {
            if has_wildcard || !function.within_group.is_empty() || arg_exprs.len() != 1 {
                return Err(InvalidQuery::new("ARRAY_AGG() takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_exprs[0], from_scope, ctx, None)?;
            let dt = e.data_type();
            let list_dt = DataType::List(Box::new(dt));
            list_dt
                .validate(cetra_column::MAX_NESTING_DEPTH)
                .map_err(|err| InvalidQuery::new(err.to_string(), pos))?;
            Ok((
                AggFunc::ArrayAgg { expr: e, distinct },
                filter,
                list_dt,
                true,
            ))
        }
    }
}

fn is_sortable_dt(dt: &DataType) -> bool {
    match dt {
        DataType::MapUtf8 | DataType::Events | DataType::Links => false,
        DataType::List(inner) => is_sortable_dt(inner),
        _ => true,
    }
}

fn ensure_sortable_order_by(dt: &DataType, pos: Option<Position>) -> Result<(), QueryError> {
    if !is_sortable_dt(dt) {
        return Err(InvalidQuery::new(
            format!("ORDER BY does not support `{}`", dt.display_name()),
            pos,
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Select-List UNNEST Extraction
// ---------------------------------------------------------------------------

#[derive(Default)]
struct UnnestCollector {
    exprs: Vec<(String, SqlExpr)>,
}

fn extract_select_unnests(
    ast: &mut SqlExpr,
    collector: &mut UnnestCollector,
) -> Result<(), QueryError> {
    let mut stack = vec![ast];
    while let Some(node) = stack.pop() {
        if let SqlExpr::Function(f) = node
            && let Some(name) = bare_object_name(&f.name)
            && name == "unnest"
        {
            let pos = at(f);
            if f.over.is_some()
                || f.filter.is_some()
                || !f.within_group.is_empty()
                || f.null_treatment.is_some()
            {
                return Err(InvalidQuery::new(
                    "unnest() does not support OVER/FILTER/WITHIN GROUP",
                    pos,
                )
                .into());
            }
            let FunctionArguments::List(list) = &f.args else {
                return Err(InvalidQuery::new("unnest() requires an argument", pos).into());
            };
            if list.duplicate_treatment.is_some()
                || !list.clauses.is_empty()
                || list.args.len() != 1
            {
                return Err(
                    InvalidQuery::new("unnest() takes exactly 1 list argument", pos).into(),
                );
            }
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_ast)) = &list.args[0] else {
                return Err(InvalidQuery::new("unnest() takes 1 expression argument", pos).into());
            };
            let synth = format!("__cetra_unnest_{}", collector.exprs.len());
            collector.exprs.push((synth.clone(), arg_ast.clone()));
            *node = SqlExpr::Identifier(Ident::with_quote('"', synth));
            continue;
        }
        push_expr_children_mut(node, &mut stack);
    }
    Ok(())
}

fn apply_select_unnests(
    plan: LogicalPlan,
    mut scope: Scope,
    collector: UnnestCollector,
    ctx: &BindCtx<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let base_len = plan.schema().len();
    let mut proj_exprs: Vec<Expr> = plan
        .schema()
        .fields()
        .iter()
        .enumerate()
        .map(|(index, f)| Expr::Column {
            index,
            name: f.name.clone(),
            data_type: f.data_type.clone(),
        })
        .collect();
    let mut proj_fields = plan.schema().fields().to_vec();
    let mut out_fields = plan.schema().fields().to_vec();
    let mut unnest_cols = Vec::with_capacity(collector.exprs.len());
    let mut new_scope_cols = Vec::with_capacity(collector.exprs.len());

    for (i, (synth, arg_ast)) in collector.exprs.into_iter().enumerate() {
        let bound = bind_sql_expr(&arg_ast, &scope, ctx, None)?;
        let DataType::List(inner_dt) = bound.data_type() else {
            return Err(InvalidQuery::new(
                format!(
                    "unnest() requires a list argument, got `{}`",
                    bound.data_type().display_name()
                ),
                at(&arg_ast),
            )
            .into());
        };
        let col_idx = base_len + i;
        proj_fields.push(Field::new(synth.clone(), bound.data_type(), true));
        proj_exprs.push(bound);
        unnest_cols.push(col_idx);
        out_fields.push(Field::new(synth.clone(), (*inner_dt).clone(), true));
        new_scope_cols.push(ScopeCol {
            qualifier: None,
            name: synth,
            data_type: *inner_dt,
            nullable: true,
            index: col_idx,
            hidden_in_star: false,
        });
    }
    scope.cols.extend(new_scope_cols);

    let proj_schema = Schema::try_new_arc(proj_fields)?;
    let proj = LogicalPlan::Project {
        input: Box::new(plan),
        exprs: proj_exprs,
        schema: proj_schema,
    };
    let out_schema = Schema::try_new_arc(out_fields)?;
    Ok((
        LogicalPlan::Unnest {
            input: Box::new(proj),
            columns: unnest_cols,
            preserve_nulls: false,
            schema: out_schema,
        },
        scope,
    ))
}

// ---------------------------------------------------------------------------
// WINDOW Function Extraction & Grouping
// ---------------------------------------------------------------------------

struct WindowCollector<'a> {
    named_windows: &'a [sqlparser::ast::NamedWindowDefinition],
    specs: Vec<(WindowSpec, Field)>,
}

impl<'a> WindowCollector<'a> {
    fn new(named_windows: &'a [sqlparser::ast::NamedWindowDefinition]) -> Self {
        Self {
            named_windows,
            specs: Vec::new(),
        }
    }
}

fn extract_windows_in_expr(
    ast: &mut SqlExpr,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    collector: &mut WindowCollector<'_>,
) -> Result<(), QueryError> {
    // Iterative traversal with left-deep BinaryOp spine flattening so 512-op chains
    // never overflow the stack while still enforcing MAX_AST_DEPTH on nesting.
    let mut work: Vec<(&mut SqlExpr, usize)> = vec![(ast, 0)];
    while let Some((node, depth)) = work.pop() {
        if depth > MAX_AST_DEPTH {
            return Err(InvalidQuery::new("the expression is nested too deeply", at(node)).into());
        }
        match node {
            SqlExpr::Function(f) if f.over.is_some() => {
                let pos = at(f);
                let Some(name) = bare_object_name(&f.name) else {
                    return Err(InvalidQuery::new(
                        "qualified window function names are not supported",
                        pos,
                    )
                    .into());
                };
                if DENIED_FUNCTIONS.contains(&name.as_str()) {
                    return Err(InvalidQuery::new(
                        format!("function `{name}` is not available in the SQL console"),
                        pos,
                    )
                    .into());
                }
                let Some(kind) = classify_win_name(&name) else {
                    return Err(InvalidQuery::new(
                        format!("unknown or unsupported window function `{name}`"),
                        pos,
                    )
                    .into());
                };
                if f.filter.is_some() {
                    return Err(InvalidQuery::new(
                        "FILTER clause is not supported on window functions",
                        pos,
                    )
                    .into());
                }
                if f.null_treatment.is_some() {
                    return Err(InvalidQuery::new(
                        "NULL treatment modifier is not supported on window functions",
                        pos,
                    )
                    .into());
                }
                let over = f
                    .over
                    .as_ref()
                    .ok_or_else(|| QueryError::internal("checked f.over.is_some()"))?;
                let ast_spec = resolve_window_type(over, collector.named_windows, pos)?;

                let mut partition_by = Vec::with_capacity(ast_spec.partition_by.len());
                for pb in &ast_spec.partition_by {
                    if expr_has_window(pb) {
                        return Err(InvalidQuery::new(
                            "window functions cannot be nested inside PARTITION BY",
                            at(pb),
                        )
                        .into());
                    }
                    partition_by.push(bind_sql_expr(pb, scope, ctx, None)?);
                }

                let mut order_by = Vec::with_capacity(ast_spec.order_by.len());
                for ob in &ast_spec.order_by {
                    if ob.with_fill.is_some() {
                        return Err(InvalidQuery::new(
                            "ORDER BY WITH FILL is not supported in OVER",
                            at(ob),
                        )
                        .into());
                    }
                    if expr_has_window(&ob.expr) {
                        return Err(InvalidQuery::new(
                            "window functions cannot be nested inside window ORDER BY",
                            at(&ob.expr),
                        )
                        .into());
                    }
                    let expr = bind_sql_expr(&ob.expr, scope, ctx, None)?;
                    ensure_sortable_order_by(&expr.data_type(), at(ob))?;
                    let descending = matches!(ob.options.asc, Some(false));
                    let nulls_first = ob.options.nulls_first.unwrap_or(descending);
                    order_by.push(SortKey {
                        expr,
                        descending,
                        nulls_first,
                    });
                }

                let frame = bind_window_frame(ast_spec.window_frame.as_ref(), ctx, pos)?;
                let (func, out_dt, nullable) = bind_window_func(kind, f, scope, ctx, pos)?;
                let idx = collector.specs.len();
                let output_name = format!("__cetra_win_{idx}");
                let spec = WindowSpec {
                    partition_by,
                    order_by,
                    frame,
                    func,
                    output_name: output_name.clone(),
                };
                let field = Field::new(output_name.clone(), out_dt, nullable);
                collector.specs.push((spec, field));
                *node = SqlExpr::Identifier(Ident::with_quote('"', output_name));
            }
            SqlExpr::Function(f) => {
                if let Some(name) = bare_object_name(&f.name)
                    && matches!(
                        name.as_str(),
                        "row_number"
                            | "rank"
                            | "dense_rank"
                            | "lag"
                            | "lead"
                            | "first_value"
                            | "last_value"
                    )
                {
                    return Err(InvalidQuery::new(
                        format!("window function `{name}` requires an OVER clause"),
                        at(f),
                    )
                    .into());
                }
                if let FunctionArguments::List(list) = &mut f.args {
                    for arg in list.args.iter_mut().rev() {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            work.push((e, depth + 1));
                        }
                    }
                }
            }
            SqlExpr::BinaryOp { left, right, .. } => {
                work.push((right, depth + 1));
                work.push((left, depth));
            }
            _ => {
                let mut children = Vec::new();
                push_expr_children_mut(node, &mut children);
                for child in children.into_iter().rev() {
                    work.push((child, depth + 1));
                }
            }
        }
    }
    Ok(())
}

fn resolve_window_type(
    over: &WindowType,
    named: &[sqlparser::ast::NamedWindowDefinition],
    pos: Option<Position>,
) -> Result<AstWindowSpec, QueryError> {
    let mut visited = Vec::new();
    match over {
        WindowType::WindowSpec(spec) => merge_window_spec(spec, named, &mut visited, pos),
        WindowType::NamedWindow(ident) => resolve_named_window(ident, named, &mut visited, pos),
    }
}

fn resolve_named_window(
    ident: &Ident,
    named: &[sqlparser::ast::NamedWindowDefinition],
    visited: &mut Vec<String>,
    pos: Option<Position>,
) -> Result<AstWindowSpec, QueryError> {
    let mut cur = ident.clone();
    loop {
        let key = ident_canonical(&cur);
        if visited.iter().any(|v| v == &key) {
            return Err(InvalidQuery::new(
                format!(
                    "circular reference in WINDOW definition `{}`",
                    ident_display(&cur)
                ),
                pos,
            )
            .into());
        }
        visited.push(key);
        let Some(def) = named
            .iter()
            .find(|def| ident_matches(&ident_canonical(&def.0), &cur))
        else {
            return Err(InvalidQuery::new(
                format!("unknown window `{}`", ident_display(&cur)),
                pos,
            )
            .into());
        };
        match &def.1 {
            sqlparser::ast::NamedWindowExpr::WindowSpec(spec) => {
                return merge_window_spec(spec, named, visited, pos);
            }
            sqlparser::ast::NamedWindowExpr::NamedWindow(other) => {
                cur = other.clone();
            }
        }
    }
}

fn merge_window_spec(
    spec: &AstWindowSpec,
    named: &[sqlparser::ast::NamedWindowDefinition],
    visited: &mut Vec<String>,
    pos: Option<Position>,
) -> Result<AstWindowSpec, QueryError> {
    let Some(base_name) = &spec.window_name else {
        return Ok(spec.clone());
    };
    let mut merged = resolve_named_window(base_name, named, visited, pos)?;
    if !spec.partition_by.is_empty() {
        return Err(InvalidQuery::new(
            format!(
                "cannot override PARTITION BY clause of window `{}`",
                ident_display(base_name)
            ),
            pos,
        )
        .into());
    }
    if !merged.order_by.is_empty() && !spec.order_by.is_empty() {
        return Err(InvalidQuery::new(
            format!(
                "cannot override ORDER BY clause of window `{}`",
                ident_display(base_name)
            ),
            pos,
        )
        .into());
    }
    if merged.window_frame.is_some() && spec.window_frame.is_some() {
        return Err(InvalidQuery::new(
            format!(
                "cannot override frame clause of window `{}`",
                ident_display(base_name)
            ),
            pos,
        )
        .into());
    }
    merged.window_name = None;
    if merged.order_by.is_empty() {
        merged.order_by = spec.order_by.clone();
    }
    if merged.window_frame.is_none() {
        merged.window_frame = spec.window_frame.clone();
    }
    Ok(merged)
}

fn bind_window_frame(
    frame: Option<&sqlparser::ast::WindowFrame>,
    ctx: &BindCtx<'_>,
    pos: Option<Position>,
) -> Result<Option<(FrameBound, FrameBound)>, QueryError> {
    let Some(wf) = frame else {
        return Ok(None);
    };
    match wf.units {
        WindowFrameUnits::Groups => {
            Err(InvalidQuery::new("GROUPS window frames are not supported", pos).into())
        }
        WindowFrameUnits::Range => match (&wf.start_bound, &wf.end_bound) {
            (WindowFrameBound::Preceding(None), None | Some(WindowFrameBound::CurrentRow)) => {
                Ok(None)
            }
            (WindowFrameBound::Preceding(None), Some(WindowFrameBound::Following(None))) => {
                Ok(Some((
                    FrameBound::UnboundedPreceding,
                    FrameBound::UnboundedFollowing,
                )))
            }
            _ => Err(InvalidQuery::new(
                "non-row RANGE window frames are not supported; use ROWS BETWEEN instead",
                pos,
            )
            .into()),
        },
        WindowFrameUnits::Rows => {
            let start = map_frame_bound(&wf.start_bound, ctx, pos)?;
            let end = match &wf.end_bound {
                Some(b) => map_frame_bound(b, ctx, pos)?,
                None => FrameBound::CurrentRow,
            };
            if start == FrameBound::UnboundedFollowing {
                return Err(InvalidQuery::new(
                    "window frame start cannot be UNBOUNDED FOLLOWING",
                    pos,
                )
                .into());
            }
            if end == FrameBound::UnboundedPreceding {
                return Err(InvalidQuery::new(
                    "window frame end cannot be UNBOUNDED PRECEDING",
                    pos,
                )
                .into());
            }
            if matches!(
                (start, end),
                (FrameBound::CurrentRow, FrameBound::Preceding(_))
                    | (
                        FrameBound::Following(_),
                        FrameBound::CurrentRow | FrameBound::Preceding(_)
                    )
            ) {
                return Err(InvalidQuery::new(
                    "window frame start cannot follow window frame end",
                    pos,
                )
                .into());
            }
            Ok(Some((start, end)))
        }
    }
}

fn map_frame_bound(
    bound: &WindowFrameBound,
    ctx: &BindCtx<'_>,
    pos: Option<Position>,
) -> Result<FrameBound, QueryError> {
    match bound {
        WindowFrameBound::CurrentRow => Ok(FrameBound::CurrentRow),
        WindowFrameBound::Preceding(None) => Ok(FrameBound::UnboundedPreceding),
        WindowFrameBound::Following(None) => Ok(FrameBound::UnboundedFollowing),
        WindowFrameBound::Preceding(Some(expr)) => {
            let n = eval_frame_offset(expr, ctx, pos)?;
            Ok(FrameBound::Preceding(n))
        }
        WindowFrameBound::Following(Some(expr)) => {
            let n = eval_frame_offset(expr, ctx, pos)?;
            Ok(FrameBound::Following(n))
        }
    }
}

fn eval_frame_offset(
    expr: &SqlExpr,
    ctx: &BindCtx<'_>,
    pos: Option<Position>,
) -> Result<u64, QueryError> {
    let val = eval_const_i64_ast(expr, ctx, pos, "window frame offset")?;
    u64::try_from(val)
        .map_err(|_| InvalidQuery::new("window frame offset must be non-negative", pos).into())
}

fn bind_window_func(
    kind: WinKind,
    f: &Function,
    scope: &Scope,
    ctx: &BindCtx<'_>,
    pos: Option<Position>,
) -> Result<(WindowFunc, DataType, bool), QueryError> {
    let FunctionArguments::List(arg_list) = &f.args else {
        return Err(InvalidQuery::new("window functions require parentheses", pos).into());
    };
    if arg_list.duplicate_treatment.is_some() || !arg_list.clauses.is_empty() {
        return Err(InvalidQuery::new(
            "DISTINCT and clauses are not supported in window functions",
            pos,
        )
        .into());
    }
    let mut has_wildcard = false;
    let mut arg_asts = Vec::with_capacity(arg_list.args.len());
    for arg in &arg_list.args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => has_wildcard = true,
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                if expr_has_window(e) {
                    return Err(
                        InvalidQuery::new("window function calls cannot be nested", at(e)).into(),
                    );
                }
                arg_asts.push(e);
            }
            _ => {
                return Err(InvalidQuery::new("unsupported window function argument", pos).into());
            }
        }
    }

    match kind {
        WinKind::RowNumber | WinKind::Rank | WinKind::DenseRank => {
            if has_wildcard || !arg_asts.is_empty() {
                return Err(
                    InvalidQuery::new("ranking window functions take 0 arguments", pos).into(),
                );
            }
            let wf = match kind {
                WinKind::RowNumber => WindowFunc::RowNumber,
                WinKind::Rank => WindowFunc::Rank,
                _ => WindowFunc::DenseRank,
            };
            Ok((wf, DataType::I64, false))
        }
        WinKind::Lag | WinKind::Lead => {
            if has_wildcard || arg_asts.is_empty() || arg_asts.len() > 3 {
                return Err(InvalidQuery::new("LAG/LEAD takes 1 to 3 arguments", pos).into());
            }
            let mut val_expr = bind_sql_expr(arg_asts[0], scope, ctx, None)?;
            let offset = if let Some(off_ast) = arg_asts.get(1) {
                eval_const_i64_ast(off_ast, ctx, pos, "LAG/LEAD offset")?
            } else {
                1
            };
            let default = if let Some(def_ast) = arg_asts.get(2) {
                let def_expr = bind_sql_expr(def_ast, scope, ctx, None)?;
                let common = coerce_common_many(&[&val_expr, &def_expr], pos)?;
                val_expr = coerce_expr(val_expr, &common, pos)?;
                Some(coerce_expr(def_expr, &common, pos)?)
            } else {
                None
            };
            let dt = val_expr.data_type();
            let wf = if matches!(kind, WinKind::Lag) {
                WindowFunc::Lag {
                    expr: val_expr,
                    offset,
                    default,
                }
            } else {
                WindowFunc::Lead {
                    expr: val_expr,
                    offset,
                    default,
                }
            };
            Ok((wf, dt, true))
        }
        WinKind::FirstValue | WinKind::LastValue => {
            if has_wildcard || arg_asts.len() != 1 {
                return Err(
                    InvalidQuery::new("FIRST_VALUE/LAST_VALUE takes 1 argument", pos).into(),
                );
            }
            let e = bind_sql_expr(arg_asts[0], scope, ctx, None)?;
            let dt = e.data_type();
            let wf = if matches!(kind, WinKind::FirstValue) {
                WindowFunc::FirstValue(e)
            } else {
                WindowFunc::LastValue(e)
            };
            Ok((wf, dt, true))
        }
        WinKind::Count => {
            if has_wildcard {
                if arg_list.args.len() != 1 {
                    return Err(InvalidQuery::new("COUNT(*) takes no other arguments", pos).into());
                }
                return Ok((WindowFunc::Count(None), DataType::I64, false));
            }
            if arg_asts.len() != 1 {
                return Err(InvalidQuery::new("COUNT() takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_asts[0], scope, ctx, None)?;
            Ok((WindowFunc::Count(Some(e)), DataType::I64, false))
        }
        WinKind::Sum => {
            if has_wildcard || arg_asts.len() != 1 {
                return Err(InvalidQuery::new("SUM() takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_asts[0], scope, ctx, None)?;
            let dt = match e.data_type() {
                DataType::I64 => DataType::I64,
                DataType::F64 => DataType::F64,
                DataType::Decimal128 { scale, .. } => DataType::Decimal128 {
                    precision: 38,
                    scale,
                },
                other => {
                    return Err(InvalidQuery::new(
                        format!(
                            "SUM() requires numeric input, got `{}`",
                            other.display_name()
                        ),
                        pos,
                    )
                    .into());
                }
            };
            Ok((WindowFunc::Sum(e), dt, true))
        }
        WinKind::Avg => {
            if has_wildcard || arg_asts.len() != 1 {
                return Err(InvalidQuery::new("AVG() takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_asts[0], scope, ctx, None)?;
            let dt = match e.data_type() {
                DataType::I64 | DataType::F64 => DataType::F64,
                DataType::Decimal128 { precision, scale } => {
                    let out_p = precision.saturating_add(4).min(38);
                    let out_s = scale.saturating_add(4).min(out_p as i8);
                    DataType::Decimal128 {
                        precision: out_p,
                        scale: out_s,
                    }
                }
                other => {
                    return Err(InvalidQuery::new(
                        format!(
                            "AVG() requires numeric input, got `{}`",
                            other.display_name()
                        ),
                        pos,
                    )
                    .into());
                }
            };
            Ok((WindowFunc::Avg(e), dt, true))
        }
        WinKind::Min | WinKind::Max => {
            if has_wildcard || arg_asts.len() != 1 {
                return Err(InvalidQuery::new("MIN/MAX takes 1 argument", pos).into());
            }
            let e = bind_sql_expr(arg_asts[0], scope, ctx, None)?;
            let dt = e.data_type();
            if !is_sortable_dt(&dt) {
                return Err(InvalidQuery::new(
                    format!("MIN/MAX does not support `{}`", dt.display_name()),
                    pos,
                )
                .into());
            }
            let wf = if matches!(kind, WinKind::Min) {
                WindowFunc::Min(e)
            } else {
                WindowFunc::Max(e)
            };
            Ok((wf, dt, true))
        }
    }
}

fn apply_window_groups(
    mut plan: LogicalPlan,
    mut scope: Scope,
    collector: WindowCollector<'_>,
) -> Result<(LogicalPlan, Scope), QueryError> {
    let mut groups: Vec<WindowGroup> = Vec::new();
    for (spec, field) in collector.specs {
        if let Some(group) = groups
            .iter_mut()
            .find(|g| g.partition_by == spec.partition_by && g.order_by == spec.order_by)
        {
            group.items.push((spec, field));
        } else {
            let partition_by = spec.partition_by.clone();
            let order_by = spec.order_by.clone();
            groups.push(WindowGroup {
                partition_by,
                order_by,
                items: vec![(spec, field)],
            });
        }
    }

    for group in groups {
        let base_len = plan.schema().len();
        let mut fields = plan.schema().fields().to_vec();
        let mut specs = Vec::with_capacity(group.items.len());
        for (offset, (spec, field)) in group.items.into_iter().enumerate() {
            fields.push(field.clone());
            scope.cols.push(ScopeCol {
                qualifier: None,
                name: spec.output_name.clone(),
                data_type: field.data_type,
                nullable: field.nullable,
                index: base_len + offset,
                hidden_in_star: false,
            });
            specs.push(spec);
        }
        let schema = Schema::try_new_arc(fields)?;
        plan = LogicalPlan::Window {
            input: Box::new(plan),
            partition_by: group.partition_by,
            order_by: group.order_by,
            specs,
            schema,
        };
    }

    Ok((plan, scope))
}

// ---------------------------------------------------------------------------
// ORDER BY & LIMIT / OFFSET
// ---------------------------------------------------------------------------

fn is_select_ordinal(mut expr: &SqlExpr, select_len: usize) -> bool {
    while let SqlExpr::Nested(inner) = expr {
        expr = inner;
    }
    if let SqlExpr::Value(vws) = expr
        && let Value::Number(s, false) = &vws.value
        && let Ok(ord) = s.parse::<usize>()
    {
        return (1..=select_len).contains(&ord);
    }
    false
}

fn unwrap_bare_ident(mut expr: &SqlExpr) -> Option<&Ident> {
    while let SqlExpr::Nested(inner) = expr {
        expr = inner;
    }
    if let SqlExpr::Identifier(ident) = expr {
        Some(ident)
    } else {
        None
    }
}

fn resolve_select_order_expr(
    ast: &SqlExpr,
    select_bound: &[BoundSelectItem],
    select_aliases: &[(String, Expr)],
    scope: &Scope,
    ctx: &BindCtx<'_>,
) -> Result<Expr, QueryError> {
    let mut cur = ast;
    while let SqlExpr::Nested(inner) = cur {
        cur = inner;
    }
    if let SqlExpr::Value(vws) = cur
        && let Value::Number(s, false) = &vws.value
        && let Ok(ord) = s.parse::<usize>()
    {
        if ord == 0 || ord > select_bound.len() {
            return Err(InvalidQuery::new(
                format!("ORDER BY position {ord} is not in select list"),
                at(ast),
            )
            .into());
        }
        return Ok(select_bound[ord - 1].expr.clone());
    }

    if let SqlExpr::Identifier(ident) = cur {
        let mut alias_matches = select_bound
            .iter()
            .filter(|b| ident_matches(&b.name, ident));
        if let Some(first) = alias_matches.next() {
            if alias_matches.next().is_some() {
                return Err(InvalidQuery::new(
                    format!("ORDER BY `{}` is ambiguous", ident_display(ident)),
                    at_ident(ident),
                )
                .into());
            }
            return Ok(first.expr.clone());
        }
    }

    bind_sql_expr(ast, scope, ctx, Some(select_aliases))
}

fn resolve_distinct_order_expr(
    ast: &SqlExpr,
    qual_hint: Option<&QualHint>,
    select_bound: &[BoundSelectItem],
    select_aliases: &[(String, Expr)],
    scope: &Scope,
    ctx: &BindCtx<'_>,
    select_schema: &Schema,
) -> Result<Expr, QueryError> {
    let mut cur = ast;
    while let SqlExpr::Nested(inner) = cur {
        cur = inner;
    }
    if let SqlExpr::Value(vws) = cur
        && let Value::Number(s, false) = &vws.value
        && let Ok(ord) = s.parse::<usize>()
    {
        if ord == 0 || ord > select_schema.len() {
            return Err(InvalidQuery::new(
                format!("ORDER BY position {ord} is not in select list"),
                at(ast),
            )
            .into());
        }
        let f = &select_schema.fields()[ord - 1];
        return Ok(Expr::Column {
            index: ord - 1,
            name: f.name.clone(),
            data_type: f.data_type.clone(),
        });
    }

    if let Some((q_opt, name)) = qual_hint {
        let matches: Vec<usize> = select_bound
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                if let Some(q) = q_opt {
                    b.source_hint.as_ref().is_some_and(|(sq, sn)| {
                        sq.as_ref()
                            .is_some_and(|sq_str| sq_str.eq_ignore_ascii_case(q))
                            && sn.eq_ignore_ascii_case(name)
                    })
                } else {
                    b.name.eq_ignore_ascii_case(name)
                }
            })
            .map(|(i, _)| i)
            .collect();
        if matches.len() == 1 {
            let idx = matches[0];
            let f = &select_schema.fields()[idx];
            return Ok(Expr::Column {
                index: idx,
                name: f.name.clone(),
                data_type: f.data_type.clone(),
            });
        }
        if matches.len() > 1 {
            return Err(
                InvalidQuery::new(format!("ORDER BY `{name}` is ambiguous"), at(ast)).into(),
            );
        }
    }

    let bound_ob = bind_sql_expr(ast, scope, ctx, Some(select_aliases))?;
    if let Some(idx) = select_bound.iter().position(|b| b.expr == bound_ob) {
        let f = &select_schema.fields()[idx];
        return Ok(Expr::Column {
            index: idx,
            name: f.name.clone(),
            data_type: f.data_type.clone(),
        });
    }

    Err(InvalidQuery::new(
        "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
        at(ast),
    )
    .into())
}

fn apply_order_by_plain(
    plan: LogicalPlan,
    order_by: &OrderBy,
    ctx: &BindCtx<'_>,
) -> Result<LogicalPlan, QueryError> {
    let OrderByKind::Expressions(exprs) = &order_by.kind else {
        return Err(InvalidQuery::new("ORDER BY ALL is not supported", at(order_by)).into());
    };
    if order_by.interpolate.is_some() {
        return Err(
            InvalidQuery::new("ORDER BY INTERPOLATE is not supported", at(order_by)).into(),
        );
    }
    let scope = Scope::from_schema(plan.schema(), None);
    let mut keys = Vec::with_capacity(exprs.len());
    for ob in exprs {
        if ob.with_fill.is_some() {
            return Err(InvalidQuery::new("ORDER BY WITH FILL is not supported", at(ob)).into());
        }
        let mut cur = &ob.expr;
        while let SqlExpr::Nested(inner) = cur {
            cur = inner;
        }
        let expr = if let SqlExpr::Value(vws) = cur
            && let Value::Number(s, false) = &vws.value
            && let Ok(ord) = s.parse::<usize>()
        {
            if ord == 0 || ord > plan.schema().len() {
                return Err(InvalidQuery::new(
                    format!("ORDER BY position {ord} is not in select list"),
                    at(ob),
                )
                .into());
            }
            let f = &plan.schema().fields()[ord - 1];
            Expr::Column {
                index: ord - 1,
                name: f.name.clone(),
                data_type: f.data_type.clone(),
            }
        } else {
            bind_sql_expr(&ob.expr, &scope, ctx, None)?
        };
        ensure_sortable_order_by(&expr.data_type(), at(ob))?;
        let descending = matches!(ob.options.asc, Some(false));
        let nulls_first = ob.options.nulls_first.unwrap_or(descending);
        keys.push(SortKey {
            expr,
            descending,
            nulls_first,
        });
    }
    Ok(LogicalPlan::Sort {
        input: Box::new(plan),
        keys,
    })
}

fn apply_limit_clause(
    plan: LogicalPlan,
    limit_clause: &LimitClause,
    ctx: &BindCtx<'_>,
) -> Result<LogicalPlan, QueryError> {
    let (limit_ast, offset_ast) = match limit_clause {
        LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            if !limit_by.is_empty() {
                return Err(
                    InvalidQuery::new("LIMIT ... BY is not supported", at(&limit_by[0])).into(),
                );
            }
            (limit.as_ref(), offset.as_ref().map(|o| &o.value))
        }
        LimitClause::OffsetCommaLimit { offset, limit } => (Some(limit), Some(offset)),
    };

    let fetch = if let Some(l_ast) = limit_ast {
        let v = eval_const_i64_ast(l_ast, ctx, at(l_ast), "LIMIT")?;
        if v < 0 {
            return Err(InvalidQuery::new("LIMIT must not be negative", at(l_ast)).into());
        }
        Some(usize::try_from(v).unwrap_or(usize::MAX))
    } else {
        None
    };

    let skip = if let Some(o_ast) = offset_ast {
        let v = eval_const_i64_ast(o_ast, ctx, at(o_ast), "OFFSET")?;
        if v < 0 {
            return Err(InvalidQuery::new("OFFSET must not be negative", at(o_ast)).into());
        }
        usize::try_from(v).unwrap_or(usize::MAX)
    } else {
        0
    };

    if skip == 0 && fetch.is_none() {
        return Ok(plan);
    }

    Ok(LogicalPlan::Limit {
        input: Box::new(plan),
        skip,
        fetch,
    })
}

fn eval_const_i64_ast(
    ast: &SqlExpr,
    ctx: &BindCtx<'_>,
    pos: Option<Position>,
    label: &str,
) -> Result<i64, QueryError> {
    if let Some((ph_name, ph_pos)) = unwrap_placeholder(ast) {
        let sv = coerce_placeholder_to(ctx.params, ph_name, &DataType::I64, ph_pos)?;
        return match sv {
            ScalarValue::I64(v) => Ok(v),
            ScalarValue::Null(_) => Ok(0),
            _ => Err(QueryError::internal("coerced placeholder not I64")),
        };
    }
    let empty_scope = Scope::default();
    if contains_placeholder(ast) {
        let mut cloned = clone_sql_expr(ast);
        let mut prebound = HashMap::new();
        let mut counter = 0usize;
        precoerce_expr(
            &mut cloned,
            Some(&DataType::I64),
            &empty_scope,
            ctx,
            None,
            &mut prebound,
            &mut counter,
            0,
        )?;
        let resolver = ScopeResolver {
            scope: &empty_scope,
            outer_scopes: &[],
            params: ctx.params,
            prebound: &prebound,
            select_aliases: None,
        };
        let bound = bind_expr_with(&cloned, &resolver)?;
        return eval_const_i64_expr(&bound, pos, label);
    }
    let bound = bind_sql_expr(ast, &empty_scope, ctx, None)?;
    eval_const_i64_expr(&bound, pos, label)
}

fn eval_const_i64_expr(expr: &Expr, pos: Option<Position>, label: &str) -> Result<i64, QueryError> {
    if expr.data_type() != DataType::I64 {
        return Err(
            InvalidQuery::new(format!("{label} must be an integer expression"), pos).into(),
        );
    }
    let sv = eval_const_scalar(expr)?;
    match sv {
        ScalarValue::I64(v) => Ok(v),
        _ => Err(InvalidQuery::new(format!("{label} must not be NULL"), pos).into()),
    }
}

fn eval_const_f64_arg(
    ast: &SqlExpr,
    ctx: &BindCtx<'_>,
    pos: Option<Position>,
) -> Result<f64, QueryError> {
    if let Some((ph_name, ph_pos)) = unwrap_placeholder(ast) {
        let sv = coerce_placeholder_to(ctx.params, ph_name, &DataType::F64, ph_pos)?;
        return match sv {
            ScalarValue::F64(v) => Ok(v),
            ScalarValue::Null(_) => Ok(0.5),
            _ => Err(QueryError::internal("coerced placeholder not F64")),
        };
    }
    let empty_scope = Scope::default();
    let bound = bind_sql_expr(ast, &empty_scope, ctx, None)?;
    let coerced = coerce_expr(bound, &DataType::F64, pos)?;
    let sv = eval_const_scalar(&coerced)?;
    match sv {
        ScalarValue::F64(v) => Ok(v),
        _ => Err(InvalidQuery::new("quantile argument must be a finite number", pos).into()),
    }
}

fn eval_const_scalar(expr: &Expr) -> Result<ScalarValue, QueryError> {
    let mut refs = BTreeSet::new();
    expr.referenced_columns(&mut refs);
    if !refs.is_empty() {
        return Err(InvalidQuery::new("argument must be a constant expression", None).into());
    }
    let schema = Schema::try_new_arc(Vec::new())?;
    let batch = Batch::try_new_all(schema, Vec::new(), 1)?;
    let eval_ctx = EvalContext::new(0, CancelToken::new());
    let col = eval_expr(expr, &batch, &eval_ctx, None)?;
    Ok(col.get_scalar(0)?)
}

fn push_expr_children_mut<'a>(node: &'a mut SqlExpr, stack: &mut Vec<&'a mut SqlExpr>) {
    match node {
        SqlExpr::Nested(inner)
        | SqlExpr::UnaryOp { expr: inner, .. }
        | SqlExpr::IsNull(inner)
        | SqlExpr::IsNotNull(inner)
        | SqlExpr::IsUnknown(inner)
        | SqlExpr::IsNotUnknown(inner)
        | SqlExpr::IsTrue(inner)
        | SqlExpr::IsFalse(inner)
        | SqlExpr::IsNotTrue(inner)
        | SqlExpr::IsNotFalse(inner)
        | SqlExpr::Cast { expr: inner, .. }
        | SqlExpr::Ceil { expr: inner, .. }
        | SqlExpr::Floor { expr: inner, .. }
        | SqlExpr::Extract { expr: inner, .. } => stack.push(inner),
        SqlExpr::BinaryOp { left, right, .. }
        | SqlExpr::IsDistinctFrom(left, right)
        | SqlExpr::IsNotDistinctFrom(left, right)
        | SqlExpr::Position {
            expr: left,
            r#in: right,
        } => {
            stack.push(left);
            stack.push(right);
        }
        SqlExpr::Between {
            expr, low, high, ..
        } => {
            stack.push(expr);
            stack.push(low);
            stack.push(high);
        }
        SqlExpr::InList { expr, list, .. } => {
            stack.push(expr);
            stack.extend(list.iter_mut());
        }
        SqlExpr::Like { expr, pattern, .. } | SqlExpr::ILike { expr, pattern, .. } => {
            stack.push(expr);
            stack.push(pattern);
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                stack.push(op);
            }
            for cw in conditions {
                stack.push(&mut cw.condition);
                stack.push(&mut cw.result);
            }
            if let Some(el) = else_result {
                stack.push(el);
            }
        }
        SqlExpr::Array(arr) => stack.extend(arr.elem.iter_mut()),
        SqlExpr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            stack.push(expr);
            if let Some(f) = substring_from {
                stack.push(f);
            }
            if let Some(l) = substring_for {
                stack.push(l);
            }
        }
        SqlExpr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            stack.push(expr);
            if let Some(w) = trim_what {
                stack.push(w);
            }
            if let Some(chars) = trim_characters {
                stack.extend(chars.iter_mut());
            }
        }
        SqlExpr::CompoundFieldAccess { root, access_chain } => {
            stack.push(root);
            for acc in access_chain {
                if let sqlparser::ast::AccessExpr::Subscript(sqlparser::ast::Subscript::Index {
                    index,
                }) = acc
                {
                    stack.push(index);
                }
            }
        }
        SqlExpr::JsonAccess { value, path } => {
            stack.push(value);
            for elem in &mut path.path {
                if let sqlparser::ast::JsonPathElem::Bracket { key }
                | sqlparser::ast::JsonPathElem::ColonBracket { key } = elem
                {
                    stack.push(key);
                }
            }
        }
        SqlExpr::Function(f) => {
            if let FunctionArguments::List(list) = &mut f.args {
                for arg in &mut list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        stack.push(e);
                    }
                }
            }
        }
        _ => {}
    }
}

fn at(spanned: &impl Spanned) -> Option<Position> {
    let start = spanned.span().start;
    if start.line == 0 {
        None
    } else {
        Some(position_of(start.line, start.column))
    }
}

fn at_ident(ident: &Ident) -> Option<Position> {
    let start = ident.span.start;
    if start.line == 0 {
        None
    } else {
        Some(position_of(start.line, start.column))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value as JsonValue, json};

    use super::*;
    use crate::exec::source::BatchSource;
    use crate::sql::params::parse_param_values;
    use crate::sql::validate::parse_and_validate_sql;

    fn catalog() -> SqlCatalog {
        let win = TimeWindow::new(1_000, 2_000).unwrap();
        let mut cat = SqlCatalog::new(Some(win), Some(500));
        let ds_schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("expected", DataType::Utf8, true),
        ])
        .unwrap();
        cat.register_batch_source("dataset_items", Arc::new(BatchSource::empty(ds_schema)))
            .unwrap();
        cat
    }

    fn bind_sql(sql: &str) -> Result<LogicalPlan, QueryError> {
        bind_sql_with_params(sql, json!({}))
    }

    fn bind_sql_with_params(sql: &str, params_json: JsonValue) -> Result<LogicalPlan, QueryError> {
        let cat = catalog();
        let (stmt, phs) = parse_and_validate_sql(sql, &cat.registered_names())?;
        let map: Map<String, JsonValue> = serde_json::from_value(params_json).unwrap();
        let vals = parse_param_values(&phs, &map)?;
        bind_statement(&stmt, &cat, &BoundParams::Values(&vals))
    }

    #[test]
    fn binds_all_five_shipped_web_examples() {
        let examples = [
            // 1. Cost per model per day (GROUP BY alias + ORDER BY alias)
            "SELECT date_trunc('day', start_time) AS day, model, sum(cost_total) AS cost
             FROM spans
             WHERE span_kind = 'llm' AND model IS NOT NULL
             GROUP BY day, model
             ORDER BY day, cost DESC",
            // 2. LLM latency percentiles per model
            "SELECT model,
               approx_percentile_cont(duration_ms, 0.5) AS p50_ms,
               approx_percentile_cont(duration_ms, 0.95) AS p95_ms,
               approx_percentile_cont(duration_ms, 0.99) AS p99_ms,
               count(*) AS calls
             FROM spans
             WHERE span_kind = 'llm' AND model IS NOT NULL
             GROUP BY model
             ORDER BY calls DESC",
            // 3. Traces with the most tool calls
            "SELECT trace_id, count(*) AS tool_calls
             FROM spans
             WHERE span_kind = 'tool'
             GROUP BY trace_id
             ORDER BY tool_calls DESC
             LIMIT 50",
            // 4. Average judge score per release (table-qualified ORDER BY after GROUP BY)
            "SELECT t.release, s.name, avg(s.value_num) AS avg_score, count(*) AS scores
             FROM scores s
             JOIN traces t ON s.trace_id = t.trace_id
             WHERE s.source = 'llm_judge' AND s.value_num IS NOT NULL
             GROUP BY t.release, s.name
             ORDER BY t.release, s.name",
            // 5. Users by spend
            "SELECT user_id, count(*) AS traces, sum(cost_total) AS cost
             FROM traces
             WHERE user_id IS NOT NULL
             GROUP BY user_id
             ORDER BY cost DESC
             LIMIT 50",
        ];

        for sql in examples {
            let plan = bind_sql(sql).unwrap_or_else(|e| panic!("failed:\n{sql}\nerr: {e}"));
            assert!(unique_names(plan.schema()).is_ok());
        }
    }

    #[test]
    fn binds_all_three_eval_stats_queries() {
        // 1. Bucket query with repeated `date_trunc('day', created_at)` in SELECT & GROUP BY + params
        let q1 = "SELECT evaluator_id, date_trunc('day', created_at) AS bucket, count(*) AS n, \
                  count(value_num) AS num_n, sum(value_num) AS num_sum FROM scores \
                  WHERE evaluator_id IS NOT NULL AND deleted = false AND evaluator_id IN ({e0:String}, {e1:String}) \
                  GROUP BY evaluator_id, date_trunc('day', created_at)";
        let p1 = bind_sql_with_params(q1, json!({"e0": "a", "e1": "b"})).unwrap();
        assert_eq!(p1.schema().len(), 5);

        // 2. Categorical top category with unaliased nested derived tables + row_number()
        let q2 = "SELECT evaluator_id, value_str, n FROM (\
                  SELECT evaluator_id, value_str, n, \
                  row_number() OVER (PARTITION BY evaluator_id ORDER BY n DESC, value_str ASC) AS rn \
                  FROM (SELECT evaluator_id, value_str, count(*) AS n FROM scores \
                  WHERE evaluator_id IS NOT NULL AND deleted = false AND data_type = 'categorical' \
                  AND value_str IS NOT NULL GROUP BY evaluator_id, value_str)) WHERE rn = 1";
        let p2 = bind_sql(q2).unwrap();
        assert_eq!(
            p2.schema()
                .fields()
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["evaluator_id", "value_str", "n"]
        );

        // 3. read_experiment_scores
        let q3 = "SELECT name, experiment_id, max(created_at) AS last_at FROM scores \
                  WHERE experiment_id IS NOT NULL AND deleted = false AND name IN ({n0:String}) \
                  GROUP BY name, experiment_id";
        let p3 = bind_sql_with_params(q3, json!({"n0": "relevance"})).unwrap();
        assert_eq!(p3.schema().len(), 3);
    }

    #[test]
    fn views_expand_window_visible_from_and_sessions_exact_schema() {
        let spans_plan = bind_sql("SELECT * FROM spans").unwrap();
        assert_eq!(spans_plan.schema().as_ref(), spans_view_schema().as_ref());

        let traces_plan = bind_sql("SELECT * FROM traces").unwrap();
        assert_eq!(traces_plan.schema().as_ref(), traces_view_schema().as_ref());

        let scores_plan = bind_sql("SELECT * FROM scores").unwrap();
        assert_eq!(scores_plan.schema().as_ref(), scores_view_schema().as_ref());

        let sessions_plan = bind_sql("SELECT * FROM sessions").unwrap();
        assert_eq!(
            sessions_plan.schema().as_ref(),
            sessions_view_schema().as_ref()
        );
        // Verify sessions plan wraps Aggregate(SumCostDecimal) -> Project(Cast F64, greatest duration_ns)
        let LogicalPlan::Project { input: outer, .. } = &sessions_plan else {
            panic!("expected outer Project for sessions");
        };
        let LogicalPlan::Project {
            input: post_agg,
            exprs,
            ..
        } = outer.as_ref()
        else {
            panic!("expected sessions post-agg Project");
        };
        assert!(matches!(post_agg.as_ref(), LogicalPlan::Aggregate { .. }));
        assert!(matches!(
            exprs[8],
            Expr::Cast {
                target: DataType::F64,
                ..
            }
        ));

        let ds_plan = bind_sql("SELECT * FROM dataset_items").unwrap();
        assert_eq!(ds_plan.schema().len(), 2);
    }

    #[test]
    fn binds_ctes_shadowing_values_unnest_and_set_ops() {
        // CTE shadowing base view + in-order scoping
        let cte_plan = bind_sql(
            "WITH spans AS (SELECT 1 AS n), b AS (SELECT n + 1 AS m FROM spans) SELECT m FROM b",
        )
        .unwrap();
        assert_eq!(cte_plan.schema().fields()[0].name, "m");

        // VALUES with alias & without alias
        let v1 = bind_sql("VALUES (1, 'a'), (2.5, NULL)").unwrap();
        assert_eq!(v1.schema().fields()[0].name, "column1");
        assert_eq!(v1.schema().fields()[0].data_type, DataType::F64);
        assert_eq!(v1.schema().fields()[1].name, "column2");

        let v2 =
            bind_sql("SELECT id, label FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, label)").unwrap();
        assert_eq!(v2.schema().fields()[0].name, "id");
        assert_eq!(v2.schema().fields()[1].name, "label");

        // UNNEST in SELECT and in FROM
        let u1 = bind_sql("SELECT unnest(tags) AS tag FROM traces").unwrap();
        assert_eq!(u1.schema().fields()[0].data_type, DataType::Utf8);

        let u2 =
            bind_sql("SELECT t.trace_id, u.tag FROM traces t, UNNEST(t.tags) AS u(tag)").unwrap();
        assert_eq!(u2.schema().len(), 2);

        // UNION / UNION ALL / INTERSECT / EXCEPT
        assert!(matches!(
            bind_sql("SELECT 1 AS a UNION ALL SELECT 2.5").unwrap(),
            LogicalPlan::Union { all: true, .. }
        ));
        assert!(matches!(
            bind_sql("SELECT 1 AS a INTERSECT SELECT 1").unwrap(),
            LogicalPlan::Join {
                kind: LogicalJoinKind::Semi,
                null_equals_null: true,
                ..
            }
        ));
        assert!(matches!(
            bind_sql("SELECT 1 AS a EXCEPT SELECT 2").unwrap(),
            LogicalPlan::Join {
                kind: LogicalJoinKind::Anti,
                null_equals_null: true,
                ..
            }
        ));
    }

    #[test]
    fn binds_subqueries_joins_windows_aggs_and_order_by() {
        // IN / NOT IN / EXISTS / NOT EXISTS
        assert!(matches!(
            bind_sql("SELECT name FROM spans WHERE trace_id IN (SELECT trace_id FROM traces)")
                .unwrap(),
            LogicalPlan::Project { .. }
        ));
        let not_in =
            bind_sql("SELECT name FROM spans WHERE trace_id NOT IN (SELECT trace_id FROM traces)")
                .unwrap();
        let LogicalPlan::Project { input, .. } = not_in else {
            panic!("expected Project");
        };
        assert!(matches!(
            input.as_ref(),
            LogicalPlan::Join {
                kind: LogicalJoinKind::AntiNullAware,
                ..
            }
        ));

        let exists =
            bind_sql("SELECT name FROM spans WHERE EXISTS (SELECT 1 FROM traces)").unwrap();
        let LogicalPlan::Project { input: ex_in, .. } = exists else {
            panic!("expected Project");
        };
        assert!(matches!(
            ex_in.as_ref(),
            LogicalPlan::Join {
                kind: LogicalJoinKind::Semi,
                ..
            }
        ));

        // Joins: INNER, LEFT, RIGHT, FULL, USING, NestedJoin, non-equi INNER
        bind_sql("SELECT a.span_id FROM spans a FULL OUTER JOIN traces b USING (trace_id)")
            .unwrap();
        bind_sql("SELECT a.span_id, b.name FROM (spans a INNER JOIN traces b ON a.trace_id = b.trace_id)").unwrap();
        bind_sql(
            "SELECT a.span_id, b.name FROM spans a LEFT JOIN traces b ON a.trace_id = b.trace_id",
        )
        .unwrap();
        bind_sql(
            "SELECT a.span_id, b.name FROM spans a RIGHT JOIN traces b ON a.trace_id = b.trace_id",
        )
        .unwrap();
        bind_sql("SELECT a.span_id, b.name FROM spans a INNER JOIN traces b ON a.start_time < b.end_time").unwrap();

        // GROUP BY ordinal + HAVING + HAVING subquery + agg inside Window + FILTER (WHERE ...) + percentile_cont DESC
        let pct = bind_sql(
            "SELECT model, percentile_cont(0.25) WITHIN GROUP (ORDER BY duration_ms DESC) AS p75, \
             count(*) FILTER (WHERE status = 'error') AS errs, \
             bool_or(status = 'error') AS any_err, bool_and(duration_ns > 0) AS all_pos, \
             array_agg(DISTINCT name) AS names, \
             coalesce(sum(cost_total), 0) AS safe_cost, \
             row_number() OVER (ORDER BY count(*) DESC) AS rn \
             FROM spans GROUP BY 1 HAVING count(*) > 1 ORDER BY 1 NULLS FIRST",
        )
        .unwrap();
        assert_eq!(pct.schema().len(), 8);

        bind_sql(
            "SELECT model, count(*) AS n FROM spans GROUP BY model \
             HAVING model IN (SELECT model FROM traces, UNNEST(models) AS m(model))",
        )
        .unwrap();

        // DISTINCT with valid ORDER BY
        bind_sql("SELECT DISTINCT t.release, s.name FROM scores s JOIN traces t ON s.trace_id = t.trace_id ORDER BY t.release, s.name").unwrap();
        bind_sql("SELECT DISTINCT model AS m FROM spans ORDER BY m").unwrap();
        bind_sql("SELECT DISTINCT model FROM spans ORDER BY 1").unwrap();

        // Named WINDOW
        bind_sql("SELECT name, row_number() OVER w AS rn FROM spans WINDOW w AS (PARTITION BY span_kind ORDER BY start_time)").unwrap();

        // VALUES with ORDER BY + LIMIT
        bind_sql("VALUES (2, 'b'), (1, 'a') ORDER BY column1 LIMIT 1").unwrap();

        // ORDER BY non-selected column retained through Sort then trimmed
        let ord_non_sel =
            bind_sql("SELECT name FROM spans ORDER BY start_time DESC LIMIT 10 OFFSET 5").unwrap();
        assert_eq!(ord_non_sel.schema().len(), 1);
        assert_eq!(ord_non_sel.schema().fields()[0].name, "name");
        assert!(matches!(ord_non_sel, LogicalPlan::Project { .. }));

        // PostgreSQL default null order: ASC -> nulls_first = false, DESC -> nulls_first = true
        let ord_defaults =
            bind_sql("SELECT name FROM spans ORDER BY name, duration_ns DESC").unwrap();
        let LogicalPlan::Project {
            input: ord_sort, ..
        } = ord_defaults
        else {
            panic!("expected Trim Project over Sort");
        };
        let LogicalPlan::Sort { keys, .. } = ord_sort.as_ref() else {
            panic!("expected Sort");
        };
        assert!(!keys[0].descending && !keys[0].nulls_first);
        assert!(keys[1].descending && keys[1].nulls_first);

        // Window functions with ROWS BETWEEN
        bind_sql(
            "SELECT name, rank() OVER (PARTITION BY span_kind ORDER BY duration_ns DESC), \
             dense_rank() OVER (ORDER BY start_time), \
             lag(duration_ns, 1, 0) OVER (ORDER BY start_time), \
             lead(name) OVER (ORDER BY start_time), \
             first_value(name) OVER (ORDER BY start_time ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
             last_value(name) OVER (ORDER BY start_time ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING), \
             sum(duration_ns) OVER (ORDER BY start_time) \
             FROM spans",
        )
        .unwrap();

        // EXPLAIN
        let expl = bind_sql("EXPLAIN SELECT name FROM spans").unwrap();
        assert_eq!(expl.schema().as_ref(), explain_schema().as_ref());
    }

    #[test]
    fn usage_site_parameter_coercion_and_rejections() {
        // String param coerced to I64 in `span_count = {s:String}`
        bind_sql_with_params(
            "SELECT trace_id FROM traces WHERE span_count = {s:String}",
            json!({"s": "2"}),
        )
        .unwrap();

        // Bad String param in `span_count = {s:String}` -> InvalidRequest
        let err = bind_sql_with_params(
            "SELECT trace_id FROM traces WHERE span_count = {s:String}",
            json!({"s": "abc"}),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::InvalidRequest(_)), "{err:?}");

        // LIMIT parameter
        bind_sql_with_params(
            "SELECT name FROM spans ORDER BY name LIMIT {n:Int}",
            json!({"n": 5}),
        )
        .unwrap();

        // Declared mode (`sql_check` without runtime values)
        let cat = catalog();
        let (stmt, phs) = parse_and_validate_sql(
            "SELECT trace_id FROM traces WHERE span_count = {s:String} AND start_time >= {t:Timestamp}",
            &cat.registered_names(),
        )
        .unwrap();
        let plan = bind_statement(&stmt, &cat, &BoundParams::Declared(&phs)).unwrap();
        assert_eq!(plan.schema().len(), 1);

        // Privacy-safe Debug on BoundParams never exposes parameter values
        let secret_val = vec![SqlParamValue {
            name: "s".to_owned(),
            value: ScalarValue::Utf8("super-secret-prompt".to_owned()),
        }];
        let dbg = format!("{:?}", BoundParams::Values(&secret_val));
        assert!(!dbg.contains("super-secret-prompt"), "{dbg}");
        assert!(dbg.contains("Values(1)"), "{dbg}");
    }

    #[test]
    fn rejects_correlated_subqueries_ungrouped_cols_negative_limit_and_duplicate_names() {
        // Correlated subquery
        let corr = bind_sql(
            "SELECT name FROM spans s WHERE EXISTS (SELECT 1 FROM traces t WHERE t.trace_id = s.trace_id)",
        )
        .unwrap_err();
        assert!(
            matches!(corr, QueryError::Invalid(ref e) if e.message.contains("correlated")),
            "{corr:?}"
        );

        // Ungrouped column
        let ungrouped = bind_sql("SELECT name, count(*) FROM spans").unwrap_err();
        assert!(
            matches!(ungrouped, QueryError::Invalid(ref e) if e.message.contains("GROUP BY")),
            "{ungrouped:?}"
        );

        // Duplicate output names
        let dup =
            bind_sql("SELECT a.name, b.name FROM spans a JOIN spans b ON a.span_id = b.span_id")
                .unwrap_err();
        assert!(
            matches!(dup, QueryError::Invalid(ref e) if e.message == "column name `name` appears twice; rename one with AS"),
            "{dup:?}"
        );

        // Negative LIMIT / OFFSET
        let neg_lim = bind_sql("SELECT name FROM spans LIMIT -1").unwrap_err();
        assert!(matches!(neg_lim, QueryError::Invalid(_)), "{neg_lim:?}");
        let neg_off = bind_sql("SELECT name FROM spans OFFSET -1").unwrap_err();
        assert!(matches!(neg_off, QueryError::Invalid(_)), "{neg_off:?}");

        // DISTINCT + non-selected ORDER BY
        let dist_ob = bind_sql("SELECT DISTINCT model FROM spans ORDER BY start_time").unwrap_err();
        assert!(matches!(dist_ob, QueryError::Invalid(_)), "{dist_ob:?}");

        // Non-equi OUTER JOIN rejected
        assert!(
            bind_sql(
                "SELECT a.span_id FROM spans a LEFT JOIN traces b ON a.start_time < b.end_time"
            )
            .is_err()
        );

        // Window function without OVER rejected
        assert!(bind_sql("SELECT row_number() FROM spans").is_err());

        // Aggregate in WHERE rejected
        assert!(bind_sql("SELECT name FROM spans WHERE count(*) > 1").is_err());

        // Out-of-range ordinals rejected
        assert!(bind_sql("SELECT name FROM spans GROUP BY 2").is_err());
        assert!(bind_sql("SELECT name FROM spans ORDER BY 2").is_err());
    }

    #[test]
    fn rejects_cyclic_named_windows_and_merges_window_inheritance() {
        // Blocker 1: cyclic WINDOW definitions
        for sql in [
            "SELECT row_number() OVER w FROM spans WINDOW w AS w",
            "SELECT row_number() OVER a FROM spans WINDOW a AS b, b AS a",
            "SELECT row_number() OVER (a) FROM spans WINDOW a AS (b), b AS (a)",
        ] {
            let err = bind_sql(sql).unwrap_err();
            assert!(
                matches!(err, QueryError::Invalid(ref e) if e.message.contains("circular")),
                "{sql}: {err:?}"
            );
        }

        // Major 1: named window inheritance merges partition_by / order_by / window_frame
        let inherited = bind_sql(
            "SELECT name, row_number() OVER (w ORDER BY start_time) AS rn \
             FROM spans WINDOW w AS (PARTITION BY span_kind)",
        )
        .unwrap();
        let LogicalPlan::Project { input, .. } = inherited else {
            panic!("expected Project");
        };
        let LogicalPlan::Window {
            partition_by,
            order_by,
            ..
        } = input.as_ref()
        else {
            panic!("expected Window");
        };
        assert_eq!(partition_by.len(), 1);
        assert_eq!(order_by.len(), 1);

        let inherited_frame = bind_sql(
            "SELECT name, sum(duration_ns) OVER (w ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS s \
             FROM spans WINDOW w AS (PARTITION BY span_kind ORDER BY start_time)",
        )
        .unwrap();
        assert_eq!(inherited_frame.schema().len(), 2);

        // Conflicting overrides rejected
        assert!(matches!(
            bind_sql(
                "SELECT row_number() OVER (w PARTITION BY model) FROM spans WINDOW w AS (PARTITION BY span_kind)"
            ),
            Err(QueryError::Invalid(_))
        ));
        assert!(matches!(
            bind_sql(
                "SELECT row_number() OVER (w ORDER BY duration_ns) FROM spans WINDOW w AS (ORDER BY start_time)"
            ),
            Err(QueryError::Invalid(_))
        ));
        assert!(matches!(
            bind_sql(
                "SELECT sum(duration_ns) OVER (w ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
                 FROM spans WINDOW w AS (ORDER BY start_time ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)"
            ),
            Err(QueryError::Invalid(_))
        ));
    }

    #[test]
    fn handles_512_op_binary_chains_and_enforces_max_ast_depth() {
        // 512-op '+' chain without placeholders (full parse + validate + bind)
        let raw_plus_512 = format!("SELECT {} FROM spans", vec!["1"; 513].join(" + "));
        bind_sql(&raw_plus_512).unwrap();

        // 512-op '||' chain without placeholders
        let raw_concat_512 = format!("SELECT {} FROM spans", vec!["'a'"; 513].join(" || "));
        bind_sql(&raw_concat_512).unwrap();

        // 511-op '+' chain with trailing/leading placeholder (within MAX_OPERATORS = 512)
        let plus_chain = format!(
            "SELECT {} + {{p:String}} FROM spans",
            vec!["1"; 511].join(" + ")
        );
        bind_sql_with_params(&plus_chain, json!({"p": "7"})).unwrap();

        let lead_plus_chain = format!(
            "SELECT {{p:String}} + {} FROM spans",
            vec!["1"; 511].join(" + ")
        );
        bind_sql_with_params(&lead_plus_chain, json!({"p": "7"})).unwrap();

        // 511-op '||' chain with placeholder
        let concat_chain = format!(
            "SELECT {} || {{s:String}} FROM spans",
            vec!["'a'"; 511].join(" || ")
        );
        bind_sql_with_params(&concat_chain, json!({"s": "z"})).unwrap();

        // 511-op 'AND' chain with placeholder
        let and_chain = format!(
            "SELECT name FROM spans WHERE {} AND {{b:String}}",
            vec!["true"; 511].join(" AND ")
        );
        bind_sql_with_params(&and_chain, json!({"b": "true"})).unwrap();

        // 511-op '+' chain in aggregate & window queries
        let agg_chain = format!(
            "SELECT model, count(*) + {} AS c FROM spans GROUP BY model",
            vec!["1"; 511].join(" + ")
        );
        bind_sql(&agg_chain).unwrap();

        let gb_chain = format!(
            "SELECT model, {} AS c FROM spans GROUP BY model",
            vec!["1"; 512].join(" + ")
        );
        bind_sql(&gb_chain).unwrap();

        let win_chain = format!(
            "SELECT row_number() OVER (ORDER BY start_time) + {} AS r FROM spans",
            vec!["1"; 512].join(" + ")
        );
        bind_sql(&win_chain).unwrap();

        // Direct 512-op BinaryOp AST with placeholder (bypassing tokenizer operator cap)
        let cat = catalog();
        let (base_ph_stmt, phs) =
            parse_and_validate_sql("SELECT {p:String} FROM spans", &cat.registered_names())
                .unwrap();
        let Statement::Query(mut ph_512_query) = base_ph_stmt else {
            panic!("expected Query");
        };
        let mut chain_expr = SqlExpr::Value(Value::Number("1".to_owned(), false).with_empty_span());
        for _ in 0..511 {
            chain_expr = SqlExpr::BinaryOp {
                left: Box::new(chain_expr),
                op: sqlparser::ast::BinaryOperator::Plus,
                right: Box::new(SqlExpr::Value(
                    Value::Number("1".to_owned(), false).with_empty_span(),
                )),
            };
        }
        if let SetExpr::Select(ref mut sel) = *ph_512_query.body
            && let Some(SelectItem::UnnamedExpr(ph_expr)) = sel.projection.pop()
        {
            chain_expr = SqlExpr::BinaryOp {
                left: Box::new(chain_expr),
                op: sqlparser::ast::BinaryOperator::Plus,
                right: Box::new(ph_expr),
            };
            sel.projection.push(SelectItem::UnnamedExpr(chain_expr));
        }
        let map: Map<String, JsonValue> = serde_json::from_value(json!({"p": "7"})).unwrap();
        let vals = parse_param_values(&phs, &map).unwrap();
        bind_statement(
            &Statement::Query(ph_512_query),
            &cat,
            &BoundParams::Values(&vals),
        )
        .unwrap();

        // Deeply nested subqueries exceed MAX_AST_DEPTH
        let mut deep_sub = "SELECT 1 AS x".to_owned();
        for _ in 0..25 {
            deep_sub = format!("SELECT x FROM ({deep_sub})");
        }
        let err = bind_sql(&deep_sub).unwrap_err();
        assert!(matches!(err, QueryError::Invalid(_)), "{err:?}");

        // Deeply nested NestedJoin AST exceeds MAX_AST_DEPTH
        let (base_stmt, _) =
            parse_and_validate_sql("SELECT name FROM spans", &cat.registered_names()).unwrap();
        let Statement::Query(mut deep_join_query) = base_stmt else {
            panic!("expected Query");
        };
        if let SetExpr::Select(ref mut sel) = *deep_join_query.body {
            let mut twj = sel.from.pop().unwrap();
            for _ in 0..70 {
                twj = TableWithJoins {
                    relation: TableFactor::NestedJoin {
                        table_with_joins: Box::new(twj),
                        alias: None,
                    },
                    joins: Vec::new(),
                };
            }
            sel.from.push(twj);
        }
        let deep_join_stmt = Statement::Query(deep_join_query);
        let err = bind_statement(&deep_join_stmt, &cat, &BoundParams::Values(&[])).unwrap_err();
        assert!(matches!(err, QueryError::Invalid(_)), "{err:?}");

        // Deeply right-nested BinaryOp exceeds MAX_AST_DEPTH in extract_windows_in_expr & rewrite_agg_expr
        let (base_expr_stmt, _) =
            parse_and_validate_sql("SELECT 1 FROM spans", &cat.registered_names()).unwrap();
        let Statement::Query(mut right_deep_query) = base_expr_stmt else {
            panic!("expected Query");
        };
        let mut right_deep = SqlExpr::Value(Value::Number("1".to_owned(), false).with_empty_span());
        for _ in 0..70 {
            right_deep = SqlExpr::BinaryOp {
                left: Box::new(SqlExpr::Value(
                    Value::Number("1".to_owned(), false).with_empty_span(),
                )),
                op: sqlparser::ast::BinaryOperator::Plus,
                right: Box::new(right_deep),
            };
        }
        if let SetExpr::Select(ref mut sel) = *right_deep_query.body {
            sel.projection = vec![SelectItem::UnnamedExpr(right_deep)];
        }
        let err = bind_statement(
            &Statement::Query(right_deep_query),
            &cat,
            &BoundParams::Values(&[]),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn fixes_window_frames_unnest_values_distinct_and_agg_windows() {
        // Major 2: RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING with ORDER BY
        let range_plan = bind_sql(
            "SELECT last_value(name) OVER (ORDER BY start_time RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) \
             FROM spans",
        )
        .unwrap();
        let LogicalPlan::Project { input, .. } = range_plan else {
            panic!("expected Project");
        };
        let LogicalPlan::Window { specs, .. } = input.as_ref() else {
            panic!("expected Window");
        };
        assert_eq!(
            specs[0].frame,
            Some((
                FrameBound::UnboundedPreceding,
                FrameBound::UnboundedFollowing
            ))
        );

        let default_range = bind_sql(
            "SELECT last_value(name) OVER (ORDER BY start_time RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
             FROM spans",
        )
        .unwrap();
        let LogicalPlan::Project { input: dr_in, .. } = default_range else {
            panic!("expected Project");
        };
        let LogicalPlan::Window {
            specs: dr_specs, ..
        } = dr_in.as_ref()
        else {
            panic!("expected Window");
        };
        assert_eq!(dr_specs[0].frame, None);

        // Major 3: multi-arg UNNEST does not mutate left_scope mid-loop
        let multi_unnest =
            bind_sql("SELECT u.tags, u.t2 FROM traces, UNNEST(tags, tags) AS u(tags, t2)").unwrap();
        assert_eq!(multi_unnest.schema().len(), 2);

        // Major 4: VALUES preserves nullability of non-literal nullable expressions
        let vals = bind_sql("VALUES (TRY_CAST('abc' AS BIGINT))").unwrap();
        assert!(vals.schema().fields()[0].nullable);
        let non_null_vals = bind_sql("VALUES (1), (2)").unwrap();
        assert!(!non_null_vals.schema().fields()[0].nullable);

        // Major 5: SELECT DISTINCT ORDER BY expressions & GROUP BY + DISTINCT
        bind_sql("SELECT DISTINCT lower(model) FROM spans ORDER BY lower(model)").unwrap();
        bind_sql("SELECT DISTINCT t.release FROM traces t GROUP BY t.release ORDER BY t.release")
            .unwrap();

        // Major 6: aggregate inside OVER and named WINDOW definitions
        bind_sql(
            "SELECT model, row_number() OVER w AS rn FROM spans GROUP BY model WINDOW w AS (ORDER BY model)",
        )
        .unwrap();
        bind_sql(
            "SELECT model, row_number() OVER w AS rn FROM spans GROUP BY model WINDOW w AS (ORDER BY count(*) DESC)",
        )
        .unwrap();
        bind_sql(
            "SELECT row_number() OVER w AS rn FROM spans WINDOW w AS (ORDER BY count(*) DESC)",
        )
        .unwrap();
        assert!(matches!(
            bind_sql("SELECT model, row_number() OVER (ORDER BY count(*) DESC) FROM spans"),
            Err(QueryError::Invalid(ref e)) if e.message.contains("GROUP BY")
        ));
    }

    #[test]
    fn enforces_cte_canonical_shadowing_duplicates_and_plan_node_budget() {
        // Major 7: quoted lowercase CTE shadows base table
        let shadowed = bind_sql(r#"WITH "spans" AS (SELECT 1 AS x) SELECT x FROM spans"#).unwrap();
        assert_eq!(shadowed.schema().len(), 1);
        assert_eq!(shadowed.schema().fields()[0].name, "x");

        // Uppercase quoted CTE remains distinct from unquoted lowercase base table
        let distinct_upper =
            bind_sql(r#"WITH "SPANS" AS (SELECT 1 AS x) SELECT name FROM spans"#).unwrap();
        assert_eq!(distinct_upper.schema().fields()[0].name, "name");

        // Minor 5: duplicate CTE names in same WITH rejected
        assert!(matches!(
            bind_sql("WITH a AS (SELECT 1 AS x), a AS (SELECT 2 AS x) SELECT * FROM a"),
            Err(QueryError::Invalid(_))
        ));
        assert!(matches!(
            bind_sql(r#"WITH "a" AS (SELECT 1 AS x), a AS (SELECT 2 AS x) SELECT * FROM a"#),
            Err(QueryError::Invalid(_))
        ));

        // Major 8: 10 chained 3-way UNION ALL CTEs rejected quickly via MAX_PLAN_NODES
        let mut ctes = vec!["c0 AS (SELECT 1 AS x)".to_owned()];
        for i in 1..=10 {
            let prev = i - 1;
            ctes.push(format!(
                "c{i} AS (SELECT x FROM c{prev} UNION ALL SELECT x FROM c{prev} UNION ALL SELECT x FROM c{prev})"
            ));
        }
        let blowup_sql = format!("WITH {} SELECT * FROM c10", ctes.join(", "));
        let err = bind_sql(&blowup_sql).unwrap_err();
        assert!(matches!(err, QueryError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn fixes_all_binder_minor_cases() {
        // Minor 1: expanded precoercion coverage
        bind_sql_with_params(
            "SELECT name FROM spans WHERE {b:String} IS TRUE",
            json!({"b": "true"}),
        )
        .unwrap();
        bind_sql_with_params(
            "SELECT trace_id FROM traces WHERE span_count = -{n:String}",
            json!({"n": "2"}),
        )
        .unwrap();
        bind_sql_with_params(
            "SELECT TRY_CAST({s:String} AS BIGINT), TRY_CAST((span_count = {n:String}) AS TEXT) FROM traces",
            json!({"s": "abc", "n": "2"}),
        )
        .unwrap();
        bind_sql_with_params(
            "SELECT CEIL({f:String}), FLOOR({f:String}), EXTRACT(YEAR FROM {ts:String}) FROM traces",
            json!({"f": "1.5", "ts": "2026-01-01T00:00:00Z"}),
        )
        .unwrap();
        bind_sql_with_params("SELECT ({j:Int}):foo FROM traces", json!({"j": 123})).unwrap();
        bind_sql_with_params(
            "SELECT trace_id FROM traces WHERE {s:String} IN (1, 2)",
            json!({"s": "2"}),
        )
        .unwrap();
        bind_sql_with_params(
            "SELECT trace_id FROM traces WHERE {s:String} BETWEEN 1 AND 3",
            json!({"s": "2"}),
        )
        .unwrap();
        bind_sql_with_params(
            "SELECT name FROM spans WHERE {s:String} IN (SELECT span_count FROM traces)",
            json!({"s": "2"}),
        )
        .unwrap();
        bind_sql_with_params(
            "SELECT coalesce(span_count, {s:String}), nullif(span_count, {s:String}), \
             greatest(span_count, {s:String}), least(span_count, {s:String}), \
             date_trunc('day', {ts:String}) FROM traces",
            json!({"s": "2", "ts": "2026-01-01T12:00:00Z"}),
        )
        .unwrap();

        // Minor 2: sessions intermediate agg_schema session_id nullable == true
        let sessions_plan = bind_sql("SELECT * FROM sessions").unwrap();
        let LogicalPlan::Project { input: outer, .. } = &sessions_plan else {
            panic!("expected outer Project");
        };
        let LogicalPlan::Project {
            input: agg_plan, ..
        } = outer.as_ref()
        else {
            panic!("expected inner Project");
        };
        assert!(agg_plan.schema().fields()[0].nullable);
        assert!(!sessions_plan.schema().fields()[0].nullable);

        // Minor 3: window frame bound validation at bind time + params in eval_frame_offset
        bind_sql_with_params(
            "SELECT sum(duration_ns) OVER (ORDER BY start_time ROWS BETWEEN {n:Int} PRECEDING AND CURRENT ROW) FROM spans",
            json!({"n": 2}),
        )
        .unwrap();
        for bad_frame in [
            "ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW",
            "ROWS BETWEEN CURRENT ROW AND UNBOUNDED PRECEDING",
            "ROWS BETWEEN CURRENT ROW AND 1 PRECEDING",
            "ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW",
        ] {
            let sql = format!(
                "SELECT sum(duration_ns) OVER (ORDER BY start_time {bad_frame}) FROM spans"
            );
            assert!(
                matches!(bind_sql(&sql), Err(QueryError::Invalid(_))),
                "{sql}"
            );
        }

        // Minor 4: INTERSECT / EXCEPT preserve left nullability
        let except_plan = bind_sql("SELECT 1 AS a EXCEPT SELECT NULL").unwrap();
        assert!(!except_plan.schema().fields()[0].nullable);
        let intersect_plan = bind_sql("SELECT 1 AS a INTERSECT SELECT NULL").unwrap();
        assert!(!intersect_plan.schema().fields()[0].nullable);

        // Minor 6: comma-UNNEST with trailing joins
        let comma_unnest_join = bind_sql(
            "SELECT t.trace_id, u.tag, s.name FROM traces t, UNNEST(t.tags) AS u(tag) \
             JOIN scores s ON t.trace_id = s.trace_id",
        )
        .unwrap();
        assert_eq!(comma_unnest_join.schema().len(), 3);

        // Minor 7: aliased NestedJoin over USING skips hidden_in_star columns
        bind_sql("SELECT j.trace_id, j.span_id FROM (spans a JOIN traces b USING (trace_id)) AS j")
            .unwrap();
        let nested_star = bind_sql(
            "SELECT j.* FROM ((SELECT trace_id, span_id FROM spans) a \
             JOIN (SELECT trace_id, user_id FROM traces) b USING (trace_id)) AS j",
        )
        .unwrap();
        assert_eq!(nested_star.schema().len(), 3);
        assert!(unique_names(nested_star.schema()).is_ok());
        let nested_full_star = bind_sql(
            "SELECT j.* FROM ((SELECT trace_id, span_id FROM spans) a \
             FULL OUTER JOIN (SELECT trace_id, user_id FROM traces) b USING (trace_id)) AS j",
        )
        .unwrap();
        assert_eq!(nested_full_star.schema().len(), 3);
        assert!(unique_names(nested_full_star.schema()).is_ok());
        bind_sql("SELECT b.trace_id FROM spans a JOIN traces b USING (trace_id)").unwrap();

        // Minor 8: contains_subquery + non-sortable ORDER BY rejected at bind time
        assert!(matches!(
            bind_sql("SELECT name FROM spans WHERE substring(name FROM 1 FOR 1) = (SELECT 'a')"),
            Err(QueryError::Invalid(_))
        ));
        assert!(matches!(
            bind_sql("SELECT name FROM spans WHERE trim(name) = (SELECT 'a')"),
            Err(QueryError::Invalid(_))
        ));
        assert!(matches!(
            bind_sql("SELECT name FROM spans WHERE attributes[(SELECT 'k')] = 'v'"),
            Err(QueryError::Invalid(_))
        ));
        for unsortable in [
            "SELECT name FROM spans ORDER BY attributes",
            "SELECT name FROM spans ORDER BY events",
            "SELECT name FROM spans ORDER BY links",
            "SELECT DISTINCT attributes FROM spans ORDER BY attributes",
        ] {
            assert!(
                matches!(bind_sql(unsortable), Err(QueryError::Invalid(_))),
                "{unsortable}"
            );
        }
    }
}
