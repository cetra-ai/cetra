use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use cetra_column::{Batch, CancelToken, DataType, Field, MAX_NESTING_DEPTH, ScalarValue, Schema};
use cetra_query::QueryError;
use cetra_query::bind::bind_expr;
use cetra_query::encode::SQL_MAX_ROWS;
use cetra_query::eval::eval_expr;
use cetra_query::exec::DEFAULT_BATCH_SIZE;
use cetra_query::ir::{CastMode, EvalContext, Expr};
use cetra_query::sql::sql_json;
use cetra_query::text::{SQL_LIMITS, TextKind, expect_end, parse_failure, parser, tokenize};

use crate::common::{
    CompareOpts, FIXED_NOW_NS, LayoutVariant, assert_clean_ctx, assert_rows_equiv,
    format_scalar_for_slt, make_exec_ctx, make_table_catalog_and_sources, run_sql_optimized,
    run_sql_unoptimized,
};

const SLT_FILES: &[&str] = &[
    "01_literals_nulls_3vl.slt",
    "02_numeric_decimal_temporal.slt",
    "03_strings_regex_json.slt",
    "04_arrays_maps_unnest.slt",
    "05_joins_subqueries.slt",
    "06_aggregates_groupby_having.slt",
    "07_setops_distinct_order.slt",
    "08_windows.slt",
    "09_sandbox_errors.slt",
];

const RESERVED_TABLES: &[&str] = &["spans", "traces", "scores", "sessions"];

const BASELINE_MEM_CAP: usize = 32 * 1024 * 1024;
const DEFAULT_SPILL_CAP: u64 = 64 * 1024 * 1024;
const TIGHT_MEM_CAP: usize = 64 * 1024;

#[derive(Debug, Clone)]
struct TableState {
    schema: Arc<Schema>,
    rows: Vec<Vec<ScalarValue>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    NoSort,
    RowSort,
}

#[derive(Debug, Default, Clone, Copy)]
struct Directives {
    metamorphic_off: bool,
    unordered_arrays: bool,
}

#[derive(Debug)]
enum SltRecord {
    StatementOk {
        line: usize,
        sql: String,
    },
    StatementError {
        line: usize,
        expected_substr: String,
        sql: String,
    },
    Query {
        line: usize,
        type_sig: Vec<char>,
        sort_mode: SortMode,
        directives: Directives,
        sql: String,
        expected_rows: Vec<Vec<String>>,
    },
}

fn parse_slt(file_name: &str, content: &str) -> Vec<SltRecord> {
    let lines: Vec<&str> = content.lines().collect();
    let mut records = Vec::new();
    let mut idx = 0usize;
    let mut directives = Directives::default();

    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            directives = Directives::default();
            idx += 1;
            continue;
        }
        if let Some(comment) = trimmed.strip_prefix('#') {
            let body = comment.trim();
            let lower = body.to_ascii_lowercase();
            if lower == "metamorphic: off"
                || lower.starts_with("metamorphic: off ")
                || lower == "metamorphic:off"
                || lower.starts_with("metamorphic:off ")
            {
                directives.metamorphic_off = true;
            } else if lower == "unordered_arrays" || lower.starts_with("unordered_arrays ") {
                directives.unordered_arrays = true;
            }
            // `# duckdb: skip ...` and general comments are intentionally ignored here.
            idx += 1;
            continue;
        }

        let header_line = idx + 1;
        let record_directives = directives;
        directives = Directives::default();

        if trimmed == "statement ok" {
            idx += 1;
            let sql_start = idx;
            while idx < lines.len() && !lines[idx].trim().is_empty() {
                idx += 1;
            }
            let sql = lines[sql_start..idx].join("\n");
            assert!(
                !sql.trim().is_empty(),
                "{file_name}:{header_line}: empty `statement ok` body"
            );
            records.push(SltRecord::StatementOk {
                line: header_line,
                sql,
            });
        } else if let Some(after) = trimmed.strip_prefix("statement error")
            && (after.is_empty() || after.starts_with(' '))
        {
            let expected_substr = after.trim().to_owned();
            idx += 1;
            let sql_start = idx;
            while idx < lines.len() && !lines[idx].trim().is_empty() {
                idx += 1;
            }
            let sql = lines[sql_start..idx].join("\n");
            assert!(
                !sql.trim().is_empty(),
                "{file_name}:{header_line}: empty `statement error` body"
            );
            records.push(SltRecord::StatementError {
                line: header_line,
                expected_substr,
                sql,
            });
        } else if let Some(rest) = trimmed.strip_prefix("query ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            assert!(
                !parts.is_empty() && parts.len() <= 2,
                "{file_name}:{header_line}: invalid query header `{trimmed}`"
            );
            let type_sig: Vec<char> = parts[0].chars().collect();
            assert!(
                !type_sig.is_empty()
                    && type_sig
                        .iter()
                        .all(|c| matches!(c, 'I' | 'R' | 'T' | 'B' | '?')),
                "{file_name}:{header_line}: invalid query type signature `{}`",
                parts[0]
            );
            let sort_mode = match parts.get(1).copied() {
                None | Some("nosort") => SortMode::NoSort,
                Some("rowsort") => SortMode::RowSort,
                Some(other) => {
                    panic!("{file_name}:{header_line}: unknown query sort mode `{other}`")
                }
            };
            idx += 1;
            let sql_start = idx;
            let mut found_sep = false;
            while idx < lines.len() {
                if lines[idx].trim() == "----" {
                    found_sep = true;
                    break;
                }
                assert!(
                    !lines[idx].trim().is_empty(),
                    "{file_name}:{header_line}: blank line before `----` separator"
                );
                idx += 1;
            }
            assert!(
                found_sep,
                "{file_name}:{header_line}: missing `----` separator in query record"
            );
            let sql = lines[sql_start..idx].join("\n");
            assert!(
                !sql.trim().is_empty(),
                "{file_name}:{header_line}: empty SQL in query record"
            );
            // Skip `----`
            idx += 1;
            let mut expected_rows = Vec::new();
            while idx < lines.len() && !lines[idx].trim().is_empty() {
                let row_line = lines[idx];
                let cells: Vec<String> = row_line.split('\t').map(str::to_owned).collect();
                assert_eq!(
                    cells.len(),
                    type_sig.len(),
                    "{file_name}:{}: expected {} tab-separated columns (type sig `{}`), got {} in `{row_line}`",
                    idx + 1,
                    type_sig.len(),
                    parts[0],
                    cells.len()
                );
                expected_rows.push(cells);
                idx += 1;
            }
            records.push(SltRecord::Query {
                line: header_line,
                type_sig,
                sort_mode,
                directives: record_directives,
                sql,
                expected_rows,
            });
        } else {
            panic!("{file_name}:{header_line}: unrecognized SLT record header `{trimmed}`");
        }
    }

    records
}

fn strip_sql_comments_and_semi(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            out.push(c);
            if c == '\'' {
                if i + 1 < chars.len() && chars[i + 1] == '\'' {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
        } else if in_double {
            out.push(c);
            if c == '"' {
                if i + 1 < chars.len() && chars[i + 1] == '"' {
                    out.push('"');
                    i += 2;
                    continue;
                }
                in_double = false;
            }
            i += 1;
        } else if c == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else {
            if c == '\'' {
                in_single = true;
            } else if c == '"' {
                in_double = true;
            }
            out.push(c);
            i += 1;
        }
    }

    out.trim().trim_end_matches(';').trim().to_owned()
}

fn split_top_level(input: &str, sep: char) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                if i + 1 < chars.len() && chars[i + 1] == '\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else if in_double {
            if c == '"' {
                if i + 1 < chars.len() && chars[i + 1] == '"' {
                    i += 2;
                    continue;
                }
                in_double = false;
            }
        } else {
            match c {
                '\'' => in_single = true,
                '"' => in_double = true,
                '(' => paren_depth += 1,
                ')' => {
                    if paren_depth == 0 {
                        return Err("unmatched `)`".to_owned());
                    }
                    paren_depth -= 1;
                }
                '[' => bracket_depth += 1,
                ']' => {
                    if bracket_depth == 0 {
                        return Err("unmatched `]`".to_owned());
                    }
                    bracket_depth -= 1;
                }
                ch if ch == sep && paren_depth == 0 && bracket_depth == 0 => {
                    let piece: String = chars[start..i].iter().collect();
                    parts.push(piece);
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }

    if in_single || in_double || paren_depth != 0 || bracket_depth != 0 {
        return Err(format!("unbalanced delimiters in `{input}`"));
    }
    let tail: String = chars[start..].iter().collect();
    parts.push(tail);
    Ok(parts)
}

fn strip_leading_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let trimmed = s.trim_start();
    if trimmed.len() < kw.len() || !trimmed.is_char_boundary(kw.len()) {
        return None;
    }
    let prefix = &trimmed[..kw.len()];
    if !prefix.eq_ignore_ascii_case(kw) {
        return None;
    }
    let rest = &trimmed[kw.len()..];
    if rest.is_empty() || rest.starts_with(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        Some(rest.trim_start())
    } else {
        None
    }
}

fn strip_trailing_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let trimmed = s.trim_end();
    if trimmed.len() < kw.len() {
        return None;
    }
    let split = trimmed.len() - kw.len();
    if !trimmed.is_char_boundary(split) {
        return None;
    }
    let suffix = &trimmed[split..];
    if !suffix.eq_ignore_ascii_case(kw) {
        return None;
    }
    let head = &trimmed[..split];
    if head.is_empty() || head.ends_with(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        Some(head.trim_end())
    } else {
        None
    }
}

fn parse_ident(input: &str) -> Result<(String, &str), String> {
    let s = input.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((idx, c)) = chars.next() {
            if c == '"' {
                if let Some(&(_, '"')) = chars.peek() {
                    out.push('"');
                    chars.next();
                } else {
                    if out.is_empty() {
                        return Err("empty quoted identifier".to_owned());
                    }
                    return Ok((out, &rest[idx + 1..]));
                }
            } else {
                out.push(c);
            }
        }
        Err("unterminated quoted identifier".to_owned())
    } else {
        let end = s
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(s.len());
        if end == 0 {
            return Err(format!("expected identifier at `{s}`"));
        }
        Ok((s[..end].to_ascii_lowercase(), &s[end..]))
    }
}

fn parse_sql_column_type(raw: &str) -> Result<DataType, String> {
    let trimmed = raw.trim();
    if let Some(inner_str) = trimmed.strip_suffix("[]") {
        let inner = parse_sql_column_type(inner_str)?;
        let list_dt = DataType::List(Box::new(inner));
        list_dt
            .validate(MAX_NESTING_DEPTH)
            .map_err(|e| e.to_string())?;
        return Ok(list_dt);
    }

    // Normalize internal runs of whitespace to single spaces and uppercase for matching.
    let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let upper = collapsed.to_ascii_uppercase();

    if let Some(inner_parens) = upper
        .strip_prefix("MAP")
        .and_then(|s| s.trim_start().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))
    {
        let args: Vec<&str> = inner_parens.split(',').map(str::trim).collect();
        if args.len() == 2
            && matches!(args[0], "VARCHAR" | "TEXT" | "STRING")
            && matches!(args[1], "VARCHAR" | "TEXT" | "STRING")
        {
            return Ok(DataType::MapUtf8);
        }
        return Err(format!("unsupported MAP type `{raw}`"));
    }

    if upper == "MAPUTF8" {
        return Ok(DataType::MapUtf8);
    }

    if let Some(rest) = upper
        .strip_prefix("DECIMAL")
        .or_else(|| upper.strip_prefix("NUMERIC"))
        .or_else(|| upper.strip_prefix("DEC"))
    {
        let rest = rest.trim();
        let (precision, scale) = if rest.is_empty() {
            (38u8, 10i8)
        } else if let Some(inner) = rest.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            let nums: Vec<&str> = inner.split(',').map(str::trim).collect();
            match nums.as_slice() {
                [p] => (
                    p.parse::<u8>()
                        .map_err(|_| format!("invalid decimal precision `{p}`"))?,
                    0i8,
                ),
                [p, s] => (
                    p.parse::<u8>()
                        .map_err(|_| format!("invalid decimal precision `{p}`"))?,
                    s.parse::<i8>()
                        .map_err(|_| format!("invalid decimal scale `{s}`"))?,
                ),
                _ => return Err(format!("invalid DECIMAL parameters in `{raw}`")),
            }
        } else {
            return Err(format!("invalid DECIMAL syntax `{raw}`"));
        };
        let dt = DataType::Decimal128 { precision, scale };
        dt.validate(MAX_NESTING_DEPTH).map_err(|e| e.to_string())?;
        return Ok(dt);
    }

    if upper.starts_with("VARCHAR(")
        || upper.starts_with("CHAR(")
        || upper.starts_with("CHARACTER VARYING(")
    {
        return Ok(DataType::Utf8);
    }

    match upper.as_str() {
        "BOOL" | "BOOLEAN" => Ok(DataType::Bool),
        "BIGINT" | "I64" | "INT" | "INTEGER" | "INT64" | "INT8" | "INT4" | "SMALLINT" => {
            Ok(DataType::I64)
        }
        "DOUBLE" | "DOUBLE PRECISION" | "F64" | "FLOAT" | "FLOAT64" | "FLOAT8" | "FLOAT32"
        | "REAL" => Ok(DataType::F64),
        "VARCHAR" | "TEXT" | "STRING" | "CHAR" | "CHARACTER VARYING" => Ok(DataType::Utf8),
        "BYTEA" | "BINARY" | "VARBINARY" | "BLOB" => Ok(DataType::Binary),
        "FIXEDBYTES8" => Ok(DataType::FixedBytes8),
        "FIXEDBYTES16" => Ok(DataType::FixedBytes16),
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" | "TIMESTAMP(9) WITH TIME ZONE" => {
            Ok(DataType::TimestampNs { utc: true })
        }
        "TIMESTAMP"
        | "TIMESTAMP WITHOUT TIME ZONE"
        | "TIMESTAMP(9)"
        | "TIMESTAMP(9) WITHOUT TIME ZONE" => Ok(DataType::TimestampNs { utc: false }),
        "DATE" => Ok(DataType::Date),
        "INTERVAL" => Ok(DataType::Interval),
        _ => Err(format!("unsupported column type `{raw}`")),
    }
}

fn exec_create_table(tables: &mut BTreeMap<String, TableState>, sql: &str, loc: &str) {
    let rest = strip_leading_keyword(sql, "CREATE")
        .and_then(|s| strip_leading_keyword(s, "TABLE"))
        .unwrap_or_else(|| panic!("{loc}: invalid CREATE TABLE syntax: `{sql}`"));

    let (if_not_exists, rest) = if let Some(after_if) = strip_leading_keyword(rest, "IF")
        .and_then(|s| strip_leading_keyword(s, "NOT"))
        .and_then(|s| strip_leading_keyword(s, "EXISTS"))
    {
        (true, after_if)
    } else {
        (false, rest)
    };

    let (table_name, rest) =
        parse_ident(rest).unwrap_or_else(|e| panic!("{loc}: invalid table name in `{sql}`: {e}"));
    let canonical = table_name.to_ascii_lowercase();
    assert!(
        !RESERVED_TABLES.contains(&canonical.as_str()),
        "{loc}: CREATE TABLE cannot use reserved view name `{canonical}`"
    );

    if tables.contains_key(&canonical) {
        if if_not_exists {
            return;
        }
        panic!("{loc}: table `{canonical}` already exists");
    }

    let body = rest
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("{loc}: expected `(...)` column list in `{sql}`"));

    let col_defs = split_top_level(body, ',')
        .unwrap_or_else(|e| panic!("{loc}: failed splitting columns in `{sql}`: {e}"));
    let mut fields = Vec::new();
    for raw_def in col_defs {
        let def = raw_def.trim();
        if def.is_empty() {
            continue;
        }
        let (col_name, after_name) = parse_ident(def)
            .unwrap_or_else(|e| panic!("{loc}: invalid column name in `{def}`: {e}"));
        let type_and_null = after_name.trim();
        let (type_str, nullable) =
            if let Some(before_null) = strip_trailing_keyword(type_and_null, "NULL") {
                if let Some(before_not) = strip_trailing_keyword(before_null, "NOT") {
                    (before_not, false)
                } else {
                    (before_null, true)
                }
            } else {
                (type_and_null, true)
            };
        let dt = parse_sql_column_type(type_str)
            .unwrap_or_else(|e| panic!("{loc}: invalid type for column `{col_name}`: {e}"));
        fields.push(Field::new(col_name, dt, nullable));
    }
    assert!(
        !fields.is_empty(),
        "{loc}: CREATE TABLE `{canonical}` must declare at least one column"
    );
    let schema = Schema::try_new_arc(fields)
        .unwrap_or_else(|e| panic!("{loc}: invalid schema for `{canonical}`: {e}"));
    tables.insert(
        canonical,
        TableState {
            schema,
            rows: Vec::new(),
        },
    );
}

fn exec_drop_table(tables: &mut BTreeMap<String, TableState>, sql: &str, loc: &str) {
    let rest = strip_leading_keyword(sql, "DROP")
        .and_then(|s| strip_leading_keyword(s, "TABLE"))
        .unwrap_or_else(|| panic!("{loc}: invalid DROP TABLE syntax: `{sql}`"));

    let (if_exists, rest) = if let Some(after_if) =
        strip_leading_keyword(rest, "IF").and_then(|s| strip_leading_keyword(s, "EXISTS"))
    {
        (true, after_if)
    } else {
        (false, rest)
    };

    let (table_name, trailing) =
        parse_ident(rest).unwrap_or_else(|e| panic!("{loc}: invalid table name in `{sql}`: {e}"));
    assert!(
        trailing.trim().is_empty(),
        "{loc}: unexpected trailing tokens in DROP TABLE: `{trailing}`"
    );
    let canonical = table_name.to_ascii_lowercase();
    let removed = tables.remove(&canonical);
    if removed.is_none() && !if_exists {
        panic!("{loc}: DROP TABLE `{canonical}` failed: table does not exist");
    }
}

fn is_bare_decimal_literal(s: &str) -> bool {
    let rest = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(s);
    if rest.is_empty() {
        return false;
    }
    let (mantissa, exp) = match rest.find(['e', 'E']) {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    if let Some(e) = exp {
        let e_digits = e
            .strip_prefix('-')
            .or_else(|| e.strip_prefix('+'))
            .unwrap_or(e);
        if e_digits.is_empty() || !e_digits.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    match mantissa.split_once('.') {
        Some((int_part, frac_part)) => {
            !(int_part.is_empty() && frac_part.is_empty())
                && int_part.bytes().all(|b| b.is_ascii_digit())
                && frac_part.bytes().all(|b| b.is_ascii_digit())
        }
        None => mantissa.bytes().all(|b| b.is_ascii_digit()),
    }
}

fn eval_raw_expr(
    expr_sql: &str,
    eval_ctx: &EvalContext,
    empty_schema: &Schema,
    one_row_batch: &Batch,
) -> Result<ScalarValue, String> {
    let tokens = tokenize(expr_sql, &SQL_LIMITS, TextKind::Sql).map_err(|e| e.to_string())?;
    let mut p = parser(tokens);
    let ast = p
        .parse_expr()
        .map_err(|e| parse_failure(e, &p, expr_sql).to_string())?;
    expect_end(&mut p, expr_sql).map_err(|e| e.to_string())?;
    let bound = bind_expr(&ast, empty_schema).map_err(|e| e.to_string())?;
    let col = eval_expr(&bound, one_row_batch, eval_ctx, None).map_err(|e| e.to_string())?;
    col.get_scalar(0).map_err(|e| e.to_string())
}

fn coerce_scalar_to_type(
    val: ScalarValue,
    target: &DataType,
    eval_ctx: &EvalContext,
    one_row_batch: &Batch,
) -> Result<ScalarValue, String> {
    if val.is_null() {
        return Ok(ScalarValue::Null(target.clone()));
    }
    if val.data_type() == *target {
        return Ok(val);
    }
    match (val, target) {
        (ScalarValue::TimestampNs { value, .. }, DataType::TimestampNs { utc }) => {
            Ok(ScalarValue::TimestampNs { value, utc: *utc })
        }
        (ScalarValue::Binary(b), DataType::FixedBytes8) => {
            let arr: [u8; 8] = b.try_into().map_err(|v: Vec<u8>| {
                format!("expected 8 bytes for FIXEDBYTES8, got {}", v.len())
            })?;
            Ok(ScalarValue::FixedBytes8(arr))
        }
        (ScalarValue::Binary(b), DataType::FixedBytes16) => {
            let arr: [u8; 16] = b.try_into().map_err(|v: Vec<u8>| {
                format!("expected 16 bytes for FIXEDBYTES16, got {}", v.len())
            })?;
            Ok(ScalarValue::FixedBytes16(arr))
        }
        (ScalarValue::List { values, .. }, DataType::List(target_inner)) => {
            let mut coerced = Vec::with_capacity(values.len());
            for elem in values {
                coerced.push(coerce_scalar_to_type(
                    elem,
                    target_inner,
                    eval_ctx,
                    one_row_batch,
                )?);
            }
            Ok(ScalarValue::List {
                item_type: (**target_inner).clone(),
                values: coerced,
            })
        }
        (other, target) => {
            let cast_expr = Expr::Cast {
                expr: Box::new(Expr::Literal(other)),
                target: target.clone(),
                mode: CastMode::Strict,
            };
            let col =
                eval_expr(&cast_expr, one_row_batch, eval_ctx, None).map_err(|e| e.to_string())?;
            col.get_scalar(0).map_err(|e| e.to_string())
        }
    }
}

fn try_extract_map_call(cell_sql: &str) -> Option<&str> {
    let rest = strip_leading_keyword(cell_sql, "MAP")?;
    rest.trim().strip_prefix('(')?.strip_suffix(')')
}

fn eval_insert_cell(
    cell_sql: &str,
    target: &DataType,
    eval_ctx: &EvalContext,
    empty_schema: &Schema,
    one_row_batch: &Batch,
) -> Result<ScalarValue, String> {
    let trimmed = cell_sql.trim();
    if trimmed.eq_ignore_ascii_case("NULL") {
        return Ok(ScalarValue::Null(target.clone()));
    }

    if let Some(map_inner) = try_extract_map_call(trimmed) {
        if *target != DataType::MapUtf8 {
            return Err(format!("MAP(...) used for non-MAP column `{target:?}`"));
        }
        let inner = map_inner.trim();
        if inner.is_empty() {
            return Ok(ScalarValue::MapUtf8(Vec::new()));
        }
        let args = split_top_level(inner, ',')?;
        // Support both `MAP('k1', 'v1', 'k2', NULL)` and DuckDB's `MAP(['k1'], ['v1'])`.
        if args.len() == 2 {
            let a0 = args[0].trim();
            if a0.starts_with('[') || strip_leading_keyword(a0, "ARRAY").is_some() {
                let keys_val = eval_insert_cell(
                    a0,
                    &DataType::List(Box::new(DataType::Utf8)),
                    eval_ctx,
                    empty_schema,
                    one_row_batch,
                )?;
                let vals_val = eval_insert_cell(
                    args[1].trim(),
                    &DataType::List(Box::new(DataType::Utf8)),
                    eval_ctx,
                    empty_schema,
                    one_row_batch,
                )?;
                let (ScalarValue::List { values: ks, .. }, ScalarValue::List { values: vs, .. }) =
                    (keys_val, vals_val)
                else {
                    return Err("MAP(keys, values) requires two list arguments".to_owned());
                };
                if ks.len() != vs.len() {
                    return Err("MAP(keys, values) list lengths differ".to_owned());
                }
                let mut entries = Vec::with_capacity(ks.len());
                for (k, v) in ks.into_iter().zip(vs) {
                    let ScalarValue::Utf8(k_str) = k else {
                        return Err("MAP keys must be non-null strings".to_owned());
                    };
                    let v_opt = match v {
                        ScalarValue::Null(_) => None,
                        ScalarValue::Utf8(s) => Some(s),
                        _ => return Err("MAP values must be nullable strings".to_owned()),
                    };
                    entries.push((k_str, v_opt));
                }
                return Ok(ScalarValue::MapUtf8(entries));
            }
        }
        if !args.len().is_multiple_of(2) {
            return Err(format!(
                "MAP(...) expects an even number of arguments, got {}",
                args.len()
            ));
        }
        let mut entries = Vec::with_capacity(args.len() / 2);
        for pair in args.as_chunks::<2>().0 {
            let k_val = eval_raw_expr(pair[0].trim(), eval_ctx, empty_schema, one_row_batch)?;
            let ScalarValue::Utf8(k_str) = k_val else {
                return Err(format!(
                    "MAP key `{}` must evaluate to non-null Utf8",
                    pair[0]
                ));
            };
            let v_val = eval_raw_expr(pair[1].trim(), eval_ctx, empty_schema, one_row_batch)?;
            let v_opt = match v_val {
                ScalarValue::Null(_) => None,
                ScalarValue::Utf8(s) => Some(s),
                other => {
                    return Err(format!(
                        "MAP value `{}` must evaluate to Utf8 or NULL, got {:?}",
                        pair[1],
                        other.data_type()
                    ));
                }
            };
            entries.push((k_str, v_opt));
        }
        return Ok(ScalarValue::MapUtf8(entries));
    }

    if matches!(target, DataType::Decimal128 { .. }) && is_bare_decimal_literal(trimmed) {
        return coerce_scalar_to_type(
            ScalarValue::Utf8(trimmed.to_owned()),
            target,
            eval_ctx,
            one_row_batch,
        );
    }

    let raw_val = eval_raw_expr(trimmed, eval_ctx, empty_schema, one_row_batch)?;
    coerce_scalar_to_type(raw_val, target, eval_ctx, one_row_batch)
}

fn exec_insert_into(tables: &mut BTreeMap<String, TableState>, sql: &str, loc: &str) {
    let rest = strip_leading_keyword(sql, "INSERT")
        .and_then(|s| strip_leading_keyword(s, "INTO"))
        .unwrap_or_else(|| panic!("{loc}: invalid INSERT INTO syntax: `{sql}`"));

    let (table_name, rest) =
        parse_ident(rest).unwrap_or_else(|e| panic!("{loc}: invalid table name in INSERT: {e}"));
    let canonical = table_name.to_ascii_lowercase();
    let table = tables
        .get_mut(&canonical)
        .unwrap_or_else(|| panic!("{loc}: INSERT INTO unknown table `{canonical}`"));

    let rest = rest.trim_start();
    let (col_indices, after_cols) = if rest.starts_with('(') {
        // Find matching closing ')', skipping double-quoted identifiers.
        let mut depth = 0usize;
        let mut in_double = false;
        let mut close_pos = None;
        let mut chars = rest.char_indices().peekable();
        while let Some((byte_idx, c)) = chars.next() {
            if in_double {
                if c == '"' {
                    if let Some(&(_, '"')) = chars.peek() {
                        chars.next();
                    } else {
                        in_double = false;
                    }
                }
            } else {
                match c {
                    '"' => in_double = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            close_pos = Some(byte_idx);
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        let end = close_pos.unwrap_or_else(|| panic!("{loc}: unclosed column list in INSERT"));
        let list_str = &rest[1..end];
        let col_parts = split_top_level(list_str, ',')
            .unwrap_or_else(|e| panic!("{loc}: invalid INSERT column list: {e}"));
        let mut idxs = Vec::with_capacity(col_parts.len());
        for part in col_parts {
            let (col_name, rem) = parse_ident(&part)
                .unwrap_or_else(|e| panic!("{loc}: invalid INSERT column `{part}`: {e}"));
            assert!(
                rem.trim().is_empty(),
                "{loc}: unexpected text after column name `{col_name}` in INSERT"
            );
            let idx = table
                .schema
                .index_of(&col_name)
                .or_else(|| {
                    table
                        .schema
                        .fields()
                        .iter()
                        .position(|f| f.name.eq_ignore_ascii_case(&col_name))
                })
                .unwrap_or_else(|| {
                    panic!("{loc}: unknown column `{col_name}` in table `{canonical}`")
                });
            idxs.push(idx);
        }
        (idxs, &rest[end + 1..])
    } else {
        ((0..table.schema.len()).collect::<Vec<_>>(), rest)
    };

    let values_body = strip_leading_keyword(after_cols, "VALUES")
        .unwrap_or_else(|| panic!("{loc}: expected VALUES in INSERT: `{sql}`"));
    let tuples = split_top_level(values_body, ',')
        .unwrap_or_else(|e| panic!("{loc}: failed splitting INSERT VALUES tuples: {e}"));

    let empty_schema = Schema::try_new_arc(Vec::new()).expect("empty schema");
    let one_row_batch =
        Batch::try_new_all(Arc::clone(&empty_schema), Vec::new(), 1).expect("empty 1-row batch");
    let eval_ctx = EvalContext::new(FIXED_NOW_NS, CancelToken::new());

    for tuple_raw in tuples {
        let tuple = tuple_raw.trim();
        let inner = tuple
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{loc}: expected `(...)` tuple in INSERT, got `{tuple}`"));
        let cell_strs = split_top_level(inner, ',')
            .unwrap_or_else(|e| panic!("{loc}: failed splitting tuple cells `{tuple}`: {e}"));
        assert_eq!(
            cell_strs.len(),
            col_indices.len(),
            "{loc}: INSERT tuple has {} values, expected {} in `{tuple}`",
            cell_strs.len(),
            col_indices.len()
        );

        let mut row: Vec<ScalarValue> = table
            .schema
            .fields()
            .iter()
            .map(|f| ScalarValue::Null(f.data_type.clone()))
            .collect();

        for (cell_sql, &col_idx) in cell_strs.iter().zip(&col_indices) {
            let field = &table.schema.fields()[col_idx];
            let val = eval_insert_cell(
                cell_sql,
                &field.data_type,
                &eval_ctx,
                &empty_schema,
                &one_row_batch,
            )
            .unwrap_or_else(|e| {
                panic!(
                    "{loc}: failed evaluating INSERT cell `{}` for column `{}` ({:?}): {e}",
                    cell_sql.trim(),
                    field.name,
                    field.data_type
                )
            });
            row[col_idx] = val;
        }

        for (field, val) in table.schema.fields().iter().zip(&row) {
            assert!(
                field.nullable || !val.is_null(),
                "{loc}: NULL value inserted into NOT NULL column `{}`",
                field.name
            );
        }
        table.rows.push(row);
    }
}

fn tables_snapshot(
    tables: &BTreeMap<String, TableState>,
) -> Vec<(String, Arc<Schema>, Vec<Vec<ScalarValue>>)> {
    tables
        .iter()
        .map(|(name, st)| (name.clone(), Arc::clone(&st.schema), st.rows.clone()))
        .collect()
}

fn split_slt_list_elements(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let bytes = inner.as_bytes();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b',' if bracket_depth == 0
                && brace_depth == 0
                && paren_depth == 0
                && bytes.get(i + 1) == Some(&b' ') =>
            {
                parts.push(&inner[start..i]);
                i += 2;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&inner[start..]);
    parts
}

fn canonicalize_list_string(cell: &str) -> String {
    let trimmed = cell.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return trimmed.to_owned();
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return "[]".to_owned();
    }
    let mut elems: Vec<String> = split_slt_list_elements(inner)
        .into_iter()
        .map(|p| canonicalize_list_string(p.trim()))
        .collect();
    elems.sort();
    format!("[{}]", elems.join(", "))
}

fn cells_match_slt(
    actual: &str,
    expected: &str,
    type_char: char,
    data_type: &DataType,
    unordered_arrays: bool,
) -> bool {
    if actual == expected {
        return true;
    }
    if type_char == 'R' && matches!(data_type, DataType::F64) {
        if let (Ok(a), Ok(b)) = (actual.parse::<f64>(), expected.parse::<f64>())
            && a.is_finite()
            && b.is_finite()
        {
            let diff = (a - b).abs();
            let tol = 1e-12_f64.max(1e-9 * a.abs().max(b.abs()));
            return diff <= tol;
        }
        return false;
    }
    if unordered_arrays && actual.starts_with('[') && expected.starts_with('[') {
        return canonicalize_list_string(actual) == canonicalize_list_string(expected);
    }
    false
}

fn run_slt_file(file_name: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/slt")
        .join(file_name);
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed reading {}: {e}", path.display()));
    let records = parse_slt(file_name, &content);

    let mut tables: BTreeMap<String, TableState> = BTreeMap::new();
    let mut query_counter = 0u64;
    let mut mode4_ran = 0usize;
    let mut mode4_ok = 0usize;
    let mut mode4_too_large = 0usize;

    for record in records {
        match record {
            SltRecord::StatementOk { line, sql } => {
                let loc = format!("{file_name}:{line}");
                let cleaned = strip_sql_comments_and_semi(&sql);
                if strip_leading_keyword(&cleaned, "CREATE").is_some() {
                    exec_create_table(&mut tables, &cleaned, &loc);
                } else if strip_leading_keyword(&cleaned, "DROP").is_some() {
                    exec_drop_table(&mut tables, &cleaned, &loc);
                } else if strip_leading_keyword(&cleaned, "INSERT").is_some() {
                    exec_insert_into(&mut tables, &cleaned, &loc);
                } else if strip_leading_keyword(&cleaned, "SELECT").is_some()
                    || strip_leading_keyword(&cleaned, "WITH").is_some()
                    || strip_leading_keyword(&cleaned, "EXPLAIN").is_some()
                    || strip_leading_keyword(&cleaned, "VALUES").is_some()
                {
                    let snap = tables_snapshot(&tables);
                    let (cat, src) =
                        make_table_catalog_and_sources(&snap, LayoutVariant::FlatAll, 0);
                    let exec =
                        make_exec_ctx(BASELINE_MEM_CAP, DEFAULT_SPILL_CAP, 1, DEFAULT_BATCH_SIZE);
                    let res = sql_json(
                        &cat,
                        &src,
                        &exec.ctx,
                        &sql,
                        &serde_json::Map::new(),
                        SQL_MAX_ROWS,
                    );
                    assert_clean_ctx(&exec.ctx);
                    res.unwrap_or_else(|e| {
                        panic!("{loc}: `statement ok` failed with error `{e}`\nSQL:\n{sql}")
                    });
                } else {
                    panic!("{loc}: unsupported `statement ok` SQL:\n{sql}");
                }
            }

            SltRecord::StatementError {
                line,
                expected_substr,
                sql,
            } => {
                let loc = format!("{file_name}:{line}");
                let snap = tables_snapshot(&tables);
                let (cat, src) = make_table_catalog_and_sources(&snap, LayoutVariant::FlatAll, 0);
                let exec =
                    make_exec_ctx(BASELINE_MEM_CAP, DEFAULT_SPILL_CAP, 1, DEFAULT_BATCH_SIZE);
                let res = sql_json(
                    &cat,
                    &src,
                    &exec.ctx,
                    &sql,
                    &serde_json::Map::new(),
                    SQL_MAX_ROWS,
                );
                assert_clean_ctx(&exec.ctx);
                let Err(err) = res else {
                    panic!("{loc}: expected `statement error`, but query succeeded\nSQL:\n{sql}");
                };
                assert!(
                    !matches!(err, QueryError::Internal(_)),
                    "{loc}: `statement error` returned forbidden Internal error: {err}\nSQL:\n{sql}"
                );
                if !expected_substr.is_empty() {
                    let msg = err.to_string();
                    assert!(
                        msg.to_lowercase().contains(&expected_substr.to_lowercase()),
                        "{loc}: error message mismatch\n  expected substr: `{expected_substr}`\n  actual error:    `{msg}`\nSQL:\n{sql}"
                    );
                }
            }

            SltRecord::Query {
                line,
                type_sig,
                sort_mode,
                directives,
                sql,
                expected_rows,
            } => {
                query_counter += 1;
                let loc = format!("{file_name}:{line}");
                let snap = tables_snapshot(&tables);

                // Mode 1: Optimized + FlatAll + 1 partition + 32 MiB
                let (cat1, src1) = make_table_catalog_and_sources(&snap, LayoutVariant::FlatAll, 0);
                let c1 = make_exec_ctx(BASELINE_MEM_CAP, DEFAULT_SPILL_CAP, 1, DEFAULT_BATCH_SIZE);
                let (schema1, rows1) = run_sql_optimized(&cat1, &src1, &c1.ctx, &sql, None)
                    .unwrap_or_else(|e| {
                        panic!("{loc}: Mode 1 (optimized FlatAll) failed: {e}\nSQL:\n{sql}")
                    });
                assert_eq!(
                    schema1.len(),
                    type_sig.len(),
                    "{loc}: output column count ({}) != type-sig length ({})\nSQL:\n{sql}",
                    schema1.len(),
                    type_sig.len()
                );

                // Check formatted Mode 1 output against expected `.slt` rows.
                let mut actual_fmt: Vec<Vec<String>> = rows1
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|cell| {
                                let s = format_scalar_for_slt(cell);
                                if directives.unordered_arrays && s.starts_with('[') {
                                    canonicalize_list_string(&s)
                                } else {
                                    s
                                }
                            })
                            .collect()
                    })
                    .collect();
                let mut expected_fmt: Vec<Vec<String>> = expected_rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|cell| {
                                if directives.unordered_arrays && cell.starts_with('[') {
                                    canonicalize_list_string(cell)
                                } else {
                                    cell.clone()
                                }
                            })
                            .collect()
                    })
                    .collect();

                if sort_mode == SortMode::RowSort {
                    actual_fmt.sort();
                    expected_fmt.sort();
                }

                assert_eq!(
                    actual_fmt.len(),
                    expected_fmt.len(),
                    "{loc}: row count mismatch (actual={}, expected={})\nSQL:\n{sql}\nactual:\n{actual_fmt:#?}\nexpected:\n{expected_fmt:#?}",
                    actual_fmt.len(),
                    expected_fmt.len()
                );
                for (row_idx, (act_row, exp_row)) in
                    actual_fmt.iter().zip(&expected_fmt).enumerate()
                {
                    for (col_idx, ((act_cell, exp_cell), &tc)) in
                        act_row.iter().zip(exp_row).zip(&type_sig).enumerate()
                    {
                        let dt = &schema1.fields()[col_idx].data_type;
                        assert!(
                            cells_match_slt(
                                act_cell,
                                exp_cell,
                                tc,
                                dt,
                                directives.unordered_arrays
                            ),
                            "{loc}: value mismatch at row {row_idx}, col {col_idx} (type `{tc}`, schema `{dt:?}`)\n  actual:   `{act_cell}`\n  expected: `{exp_cell}`\nSQL:\n{sql}"
                        );
                    }
                }

                // Also verify sql_json encoding and clean reservation cleanup on Mode 1.
                let json_res = sql_json(
                    &cat1,
                    &src1,
                    &c1.ctx,
                    &sql,
                    &serde_json::Map::new(),
                    SQL_MAX_ROWS,
                )
                .unwrap_or_else(|e| panic!("{loc}: Mode 1 sql_json failed: {e}\nSQL:\n{sql}"));
                assert_clean_ctx(&c1.ctx);
                assert_eq!(
                    json_res.columns.len(),
                    schema1.len(),
                    "{loc}: sql_json column count mismatch"
                );
                assert_eq!(
                    json_res.rows.len(),
                    rows1.len().min(SQL_MAX_ROWS),
                    "{loc}: sql_json row count mismatch"
                );

                if directives.metamorphic_off {
                    continue;
                }

                let cmp_opts = CompareOpts {
                    ordered: sort_mode == SortMode::NoSort,
                    unordered_arrays: directives.unordered_arrays,
                    ..CompareOpts::default()
                };
                let seed = query_counter.wrapping_mul(0x517c_c1b7_2722_0a95) ^ (line as u64);

                // Mode 2: Unoptimized on FlatAll
                let c2 = make_exec_ctx(BASELINE_MEM_CAP, DEFAULT_SPILL_CAP, 1, DEFAULT_BATCH_SIZE);
                let (schema2, rows2) = run_sql_unoptimized(&cat1, &src1, &c2.ctx, &sql, None)
                    .unwrap_or_else(|e| {
                        panic!("{loc}: Mode 2 (unoptimized) failed: {e}\nSQL:\n{sql}")
                    });
                assert_eq!(
                    schema2.as_ref(),
                    schema1.as_ref(),
                    "{loc}: Mode 2 schema mismatch\nSQL:\n{sql}"
                );
                assert_rows_equiv(
                    &rows1,
                    &rows2,
                    &cmp_opts,
                    &format!("{loc} Mode 2 (unoptimized vs optimized)\nSQL:\n{sql}"),
                );

                // Mode 3: Optimized on EncodedMixed
                let (cat3, src3) =
                    make_table_catalog_and_sources(&snap, LayoutVariant::EncodedMixed, seed);
                let c3 = make_exec_ctx(BASELINE_MEM_CAP, DEFAULT_SPILL_CAP, 1, DEFAULT_BATCH_SIZE);
                let (schema3, rows3) = run_sql_optimized(&cat3, &src3, &c3.ctx, &sql, None)
                    .unwrap_or_else(|e| {
                        panic!("{loc}: Mode 3 (EncodedMixed) failed: {e}\nSQL:\n{sql}")
                    });
                assert_eq!(
                    schema3.as_ref(),
                    schema1.as_ref(),
                    "{loc}: Mode 3 schema mismatch\nSQL:\n{sql}"
                );
                assert_rows_equiv(
                    &rows1,
                    &rows3,
                    &cmp_opts,
                    &format!("{loc} Mode 3 (EncodedMixed vs FlatAll)\nSQL:\n{sql}"),
                );

                // Mode 4: Optimized on MultiChunkFlat(4), 4 partitions, 64 KiB cap, batch_size = 4
                let (cat4, src4) = make_table_catalog_and_sources(
                    &snap,
                    LayoutVariant::MultiChunkFlat(4),
                    seed.wrapping_add(1),
                );
                let c4 = make_exec_ctx(TIGHT_MEM_CAP, DEFAULT_SPILL_CAP, 4, 4);
                mode4_ran += 1;
                match run_sql_optimized(&cat4, &src4, &c4.ctx, &sql, None) {
                    Ok((schema4, rows4)) => {
                        mode4_ok += 1;
                        assert_eq!(
                            schema4.as_ref(),
                            schema1.as_ref(),
                            "{loc}: Mode 4 schema mismatch\nSQL:\n{sql}"
                        );
                        assert_rows_equiv(
                            &rows1,
                            &rows4,
                            &cmp_opts,
                            &format!("{loc} Mode 4 (parallel tight-pool vs FlatAll)\nSQL:\n{sql}"),
                        );
                    }
                    Err(QueryError::TooLarge) => {
                        mode4_too_large += 1;
                        // Clean memory-cap refusal under tight 64 KiB pool is allowed.
                    }
                    Err(other) => {
                        panic!(
                            "{loc}: Mode 4 (parallel tight-pool) failed with unexpected error: {other}\nSQL:\n{sql}"
                        );
                    }
                }
            }
        }
    }

    if mode4_ran > 0 {
        assert!(
            mode4_ok > 0 && mode4_too_large * 10 <= mode4_ran,
            "{file_name}: Mode 4 skipped too many queries via TooLarge (ok={mode4_ok}, too_large={mode4_too_large}, ran={mode4_ran})"
        );
    }
}

#[test]
fn all_slt_files_are_registered() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut on_disk: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed reading {}: {e}", dir.display()))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            name.ends_with(".slt").then_some(name)
        })
        .collect();
    on_disk.sort();

    let mut registered: Vec<String> = SLT_FILES.iter().map(|&s| s.to_owned()).collect();
    registered.sort();
    assert_eq!(
        on_disk, registered,
        "every .slt file in crates/query/tests/slt/ must be registered in SLT_FILES"
    );
}

#[test]
fn slt_01_literals_nulls_3vl() {
    run_slt_file("01_literals_nulls_3vl.slt");
}

#[test]
fn slt_02_numeric_decimal_temporal() {
    run_slt_file("02_numeric_decimal_temporal.slt");
}

#[test]
fn slt_03_strings_regex_json() {
    run_slt_file("03_strings_regex_json.slt");
}

#[test]
fn slt_04_arrays_maps_unnest() {
    run_slt_file("04_arrays_maps_unnest.slt");
}

#[test]
fn slt_05_joins_subqueries() {
    run_slt_file("05_joins_subqueries.slt");
}

#[test]
fn slt_06_aggregates_groupby_having() {
    run_slt_file("06_aggregates_groupby_having.slt");
}

#[test]
fn slt_07_setops_distinct_order() {
    run_slt_file("07_setops_distinct_order.slt");
}

#[test]
fn slt_08_windows() {
    run_slt_file("08_windows.slt");
}

#[test]
fn slt_09_sandbox_errors() {
    run_slt_file("09_sandbox_errors.slt");
}
