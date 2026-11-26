//! Native `cetra-column` keyset cursors for list queries.
//!
//! A cursor is the lowercase hex of a compact JSON object. Timestamps are
//! stored as nanoseconds (`Int`). Float sort columns (`cost_total`,
//! `error_rate`, `traces_per_session`) are JSON numbers decoded as `f64` via
//! `str::parse::<f64>()` to preserve exact IEEE-754 bit round-tripping.

use std::fmt;

use cetra_column::{Batch, Column, DataType, FlatData, ScalarValue, Schema, Selection};
use cetra_query::exec::SortKey;
use cetra_query::{BinaryOp, Expr, UnaryOp};
use serde_json::{Map, Value};

use crate::query_error::RunError;
use crate::reads::SortOrder;
use crate::reads_qe::columns::{
    internal, physical_row, raw_f64_at, raw_i64_at, raw_string_at, raw_time_at, require_col,
};

/// Maximum accepted encoded cursor byte length.
pub(crate) const MAX_CURSOR_BYTES: usize = 4096;

/// One page of `cetra-column` batches and an optional opaque keyset cursor.
pub(crate) struct QePage {
    /// Rows in sort order, at most the requested limit.
    pub(crate) batches: Vec<Batch>,
    /// Cursor for the next page. `None` when this page is the last.
    pub(crate) next_cursor: Option<String>,
}

impl fmt::Debug for QePage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rows: usize = self.batches.iter().map(Batch::num_rows).sum();
        f.debug_struct("QePage")
            .field("batches_len", &self.batches.len())
            .field("rows", &rows)
            .field("has_next_cursor", &self.next_cursor.is_some())
            .finish()
    }
}

/// Decoded keyset cursor. Manual `fmt::Debug` prints `tie_len` only (never user/session IDs).
#[derive(Clone, PartialEq)]
pub(crate) struct Cursor {
    pub(crate) list: String,
    pub(crate) sort: String,
    pub(crate) order: SortOrder,
    pub(crate) value: CursorValue,
    pub(crate) tie: Vec<String>,
}

impl fmt::Debug for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor")
            .field("list", &self.list)
            .field("sort", &self.sort)
            .field("order", &self.order)
            .field("value_is_null", &matches!(self.value, CursorValue::Null))
            .field("tie_len", &self.tie.len())
            .finish()
    }
}

/// Sort-column value carried in a [`Cursor`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CursorValue {
    Null,
    Int(i64),
    Float(f64),
}

impl Cursor {
    /// Number of tie-breaker key strings carried by this cursor.
    pub(crate) fn tie_len(&self) -> usize {
        self.tie.len()
    }

    /// Lowercase hex of the compact JSON object.
    pub(crate) fn encode(&self) -> String {
        let mut object = Map::new();
        object.insert("l".to_owned(), Value::String(self.list.clone()));
        object.insert("s".to_owned(), Value::String(self.sort.clone()));
        object.insert(
            "o".to_owned(),
            Value::String(
                match self.order {
                    SortOrder::Asc => "asc",
                    SortOrder::Desc => "desc",
                }
                .to_owned(),
            ),
        );
        object.insert(
            "v".to_owned(),
            match self.value {
                CursorValue::Null => Value::Null,
                CursorValue::Int(value) => Value::from(value),
                CursorValue::Float(value) => serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            },
        );
        object.insert(
            "t".to_owned(),
            Value::Array(self.tie.iter().cloned().map(Value::String).collect()),
        );
        let Ok(bytes) = serde_json::to_vec(&Value::Object(object)) else {
            return String::new();
        };
        hex::encode(bytes)
    }
}

/// Decodes `text` for one list, sort column, and direction.
pub(crate) fn decode(
    text: &str,
    list: &str,
    sort: &str,
    order: SortOrder,
) -> Result<Cursor, RunError> {
    if text.len() > MAX_CURSOR_BYTES {
        return Err(invalid_cursor());
    }
    let bytes = hex::decode(text).map_err(|_| invalid_cursor())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
    let Some(object) = value.as_object() else {
        return Err(invalid_cursor());
    };
    let (decoded_list, decoded_sort, decoded_order, cursor_value, tie) =
        parse_object(object, &bytes)?;
    if decoded_list != list || decoded_sort != sort || decoded_order != order {
        return Err(RunError::InvalidRequest(
            "the cursor belongs to another list or sort order".to_owned(),
        ));
    }
    Ok(Cursor {
        list: decoded_list,
        sort: decoded_sort,
        order: decoded_order,
        value: cursor_value,
        tie,
    })
}

fn parse_object(
    object: &Map<String, Value>,
    raw_json: &[u8],
) -> Result<(String, String, SortOrder, CursorValue, Vec<String>), RunError> {
    let list = object
        .get("l")
        .and_then(Value::as_str)
        .ok_or_else(invalid_cursor)?;
    let sort_name = object
        .get("s")
        .and_then(Value::as_str)
        .ok_or_else(invalid_cursor)?;
    let order = match object.get("o").and_then(Value::as_str) {
        Some("asc") => SortOrder::Asc,
        Some("desc") => SortOrder::Desc,
        _ => return Err(invalid_cursor()),
    };
    let value = object.get("v").ok_or_else(invalid_cursor)?;
    let cursor_value = parse_value(sort_name, value, raw_json)?;
    let ties = object
        .get("t")
        .and_then(Value::as_array)
        .ok_or_else(invalid_cursor)?;
    let mut tie = Vec::with_capacity(ties.len());
    for item in ties {
        let Some(text) = item.as_str() else {
            return Err(invalid_cursor());
        };
        tie.push(text.to_owned());
    }
    Ok((
        list.to_owned(),
        sort_name.to_owned(),
        order,
        cursor_value,
        tie,
    ))
}

fn parse_value(sort: &str, value: &Value, raw_json: &[u8]) -> Result<CursorValue, RunError> {
    if value.is_null() {
        return Ok(CursorValue::Null);
    }
    let Some(number) = value.as_number() else {
        return Err(invalid_cursor());
    };
    if is_float_column(sort) {
        let token = json_field_token(raw_json, "v")?;
        let parsed = token.parse::<f64>().map_err(|_| invalid_cursor())?;
        if !parsed.is_finite() {
            return Err(invalid_cursor());
        }
        return Ok(CursorValue::Float(parsed));
    }
    let Some(int_val) = number.as_i64() else {
        return Err(invalid_cursor());
    };
    Ok(CursorValue::Int(int_val))
}

/// Builds `sort_col <order> NULLS LAST`, then each tie column `<order> NULLS LAST`.
pub(crate) fn order_by(
    schema: &Schema,
    sort_col: &str,
    tie_cols: &[&str],
    order: SortOrder,
) -> Result<Vec<SortKey>, RunError> {
    let descending = order == SortOrder::Desc;
    let mut sorts = Vec::with_capacity(1 + tie_cols.len());
    sorts.push(SortKey {
        expr: col_expr(schema, sort_col)?,
        descending,
        nulls_first: false,
    });
    for tie in tie_cols {
        sorts.push(SortKey {
            expr: col_expr(schema, tie)?,
            descending,
            nulls_first: false,
        });
    }
    Ok(sorts)
}

/// Builds the keyset `after` predicate over `schema` for rows strictly after `cursor`
/// in `sort_col <order> NULLS LAST, tie... <order>`.
pub(crate) fn after_expr(
    cursor: &Cursor,
    schema: &Schema,
    sort_col: &str,
    tie_cols: &[&str],
) -> Result<Expr, RunError> {
    if cursor.tie.len() != tie_cols.len() {
        return Err(invalid_cursor());
    }
    let ties = tie_after(schema, tie_cols, &cursor.tie, cursor.order)?;
    let column = col_expr(schema, sort_col)?;
    let col_dt = column.data_type();
    match cursor.value {
        CursorValue::Null => Ok(and_expr(is_null(column), ties)),
        CursorValue::Int(_) | CursorValue::Float(_) => {
            let literal = sort_literal(&col_dt, cursor.value)?;
            let cmp = compare(column.clone(), cursor.order, literal.clone());
            let eq_and_tie = and_expr(eq_expr(column.clone(), literal), ties);
            Ok(or_expr(or_expr(cmp, eq_and_tie), is_null(column)))
        }
    }
}

/// Keeps at most `limit` active rows. When more came back, `next_cursor` is the last kept row.
pub(crate) fn assemble(
    batches: Vec<Batch>,
    limit: usize,
    list: &str,
    sort: &str,
    order: SortOrder,
    tie_cols: &[&str],
) -> Result<QePage, RunError> {
    let total: usize = batches.iter().map(Batch::num_rows).sum();
    if total <= limit {
        return Ok(QePage {
            batches,
            next_cursor: None,
        });
    }
    let batches = take_rows(batches, limit)?;
    let Some((batch, active_row)) = last_row(&batches) else {
        return Err(internal("a truncated page has no last row"));
    };
    let cursor = cursor_from_row(batch, active_row, list, sort, order, tie_cols)?;
    Ok(QePage {
        batches,
        next_cursor: Some(cursor.encode()),
    })
}

fn cursor_from_row(
    batch: &Batch,
    active_row: usize,
    list: &str,
    sort: &str,
    order: SortOrder,
    tie_cols: &[&str],
) -> Result<Cursor, RunError> {
    let phys_row = physical_row(batch, active_row)?;
    let sort_col = require_col(batch, sort)?;
    let value = sort_value(sort_col, phys_row)?;
    let mut tie = Vec::with_capacity(tie_cols.len());
    for name in tie_cols {
        let col = require_col(batch, name)?;
        tie.push(tie_string_value(col, phys_row, name)?);
    }
    Ok(Cursor {
        list: list.to_owned(),
        sort: sort.to_owned(),
        order,
        value,
        tie,
    })
}

fn sort_value(col: &Column, phys_row: usize) -> Result<CursorValue, RunError> {
    if phys_row >= col.len() {
        return Err(internal("sort row index out of bounds"));
    }
    if col.is_null(phys_row) {
        return Ok(CursorValue::Null);
    }
    match col.data_type() {
        DataType::I64 => Ok(CursorValue::Int(raw_i64_at(col, phys_row)?)),
        DataType::TimestampNs { .. } => Ok(CursorValue::Int(raw_time_at(col, phys_row)?)),
        DataType::F64 => match raw_f64_at(col, phys_row)? {
            Some(v) => Ok(CursorValue::Float(v)),
            None => Ok(CursorValue::Null),
        },
        other => Err(internal(format!(
            "unsupported sort type {}",
            other.display_name()
        ))),
    }
}

fn tie_string_value(col: &Column, phys_row: usize, name: &str) -> Result<String, RunError> {
    if phys_row >= col.len() {
        return Err(internal(format!("tie row index out of bounds for {name}")));
    }
    if col.is_null(phys_row) {
        return Err(internal(format!("tie column {name} is null")));
    }
    match col.data_type() {
        DataType::Utf8 => raw_string_at(col, phys_row)?
            .ok_or_else(|| internal(format!("tie column {name} is null"))),
        DataType::FixedBytes16 => read_fixed_bytes_hex(col, phys_row, 16, name),
        DataType::FixedBytes8 => read_fixed_bytes_hex(col, phys_row, 8, name),
        other => Err(internal(format!(
            "tie column {name} has type {}",
            other.display_name()
        ))),
    }
}

fn read_fixed_bytes_hex(
    col: &Column,
    phys_row: usize,
    expected_len: usize,
    name: &str,
) -> Result<String, RunError> {
    match col {
        Column::Const { value, .. } => match value {
            ScalarValue::FixedBytes16(b) if expected_len == 16 => Ok(hex::encode(b)),
            ScalarValue::FixedBytes8(b) if expected_len == 8 => Ok(hex::encode(b)),
            _ => Err(internal(format!("tie column {name} scalar mismatch"))),
        },
        Column::Flat(fc) => read_flat_fixed_hex(fc.data(), phys_row, expected_len, name),
        Column::Dict(d) => {
            let code =
                *d.codes()
                    .get(phys_row)
                    .ok_or_else(|| internal("dict code out of bounds"))? as usize;
            read_flat_fixed_hex(d.values().data(), code, expected_len, name)
        }
        Column::Rle(r) => {
            let run = r
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            read_flat_fixed_hex(r.values().data(), run, expected_len, name)
        }
    }
}

fn read_flat_fixed_hex(
    data: &FlatData,
    idx: usize,
    expected_len: usize,
    name: &str,
) -> Result<String, RunError> {
    match data {
        FlatData::FixedBytes16(v) if expected_len == 16 => v
            .get(idx)
            .map(hex::encode)
            .ok_or_else(|| internal(format!("tie column {name} index out of bounds"))),
        FlatData::FixedBytes8(v) if expected_len == 8 => v
            .get(idx)
            .map(hex::encode)
            .ok_or_else(|| internal(format!("tie column {name} index out of bounds"))),
        _ => Err(internal(format!("tie column {name} flat storage mismatch"))),
    }
}

fn col_expr(schema: &Schema, name: &str) -> Result<Expr, RunError> {
    let index = schema
        .index_of(name)
        .ok_or_else(|| internal(format!("missing column {name}")))?;
    Ok(Expr::Column {
        index,
        name: name.to_owned(),
        data_type: schema.fields()[index].data_type.clone(),
    })
}

fn sort_literal(col_dt: &DataType, value: CursorValue) -> Result<Expr, RunError> {
    let sv = match (col_dt, value) {
        (dt, CursorValue::Null) => ScalarValue::Null(dt.clone()),
        (DataType::TimestampNs { utc }, CursorValue::Int(v)) => ScalarValue::TimestampNs {
            value: v,
            utc: *utc,
        },
        (DataType::I64, CursorValue::Int(v)) => ScalarValue::I64(v),
        (DataType::F64, CursorValue::Float(v)) => ScalarValue::F64(v),
        _ => {
            return Err(internal(format!(
                "cursor sort value does not match column type {}",
                col_dt.display_name()
            )));
        }
    };
    Ok(Expr::Literal(sv))
}

fn tie_literal(col_dt: &DataType, value: &str) -> Result<Expr, RunError> {
    let sv = match col_dt {
        DataType::Utf8 => ScalarValue::Utf8(value.to_owned()),
        DataType::FixedBytes16 => {
            let bytes = hex::decode(value).map_err(|_| invalid_cursor())?;
            let arr: [u8; 16] = bytes.try_into().map_err(|_| invalid_cursor())?;
            ScalarValue::FixedBytes16(arr)
        }
        DataType::FixedBytes8 => {
            let bytes = hex::decode(value).map_err(|_| invalid_cursor())?;
            let arr: [u8; 8] = bytes.try_into().map_err(|_| invalid_cursor())?;
            ScalarValue::FixedBytes8(arr)
        }
        other => {
            return Err(internal(format!(
                "unsupported tie column type {}",
                other.display_name()
            )));
        }
    };
    Ok(Expr::Literal(sv))
}

fn compare(left: Expr, order: SortOrder, right: Expr) -> Expr {
    let op = match order {
        SortOrder::Asc => BinaryOp::Gt,
        SortOrder::Desc => BinaryOp::Lt,
    };
    Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
        data_type: DataType::Bool,
    }
}

fn eq_expr(left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op: BinaryOp::Eq,
        right: Box::new(right),
        data_type: DataType::Bool,
    }
}

fn and_expr(left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op: BinaryOp::And,
        right: Box::new(right),
        data_type: DataType::Bool,
    }
}

fn or_expr(left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op: BinaryOp::Or,
        right: Box::new(right),
        data_type: DataType::Bool,
    }
}

fn is_null(expr: Expr) -> Expr {
    Expr::Unary {
        op: UnaryOp::IsNull,
        expr: Box::new(expr),
        data_type: DataType::Bool,
    }
}

/// Lexicographic `T <op> t` for non-null tie columns.
fn tie_after(
    schema: &Schema,
    cols: &[&str],
    values: &[String],
    order: SortOrder,
) -> Result<Expr, RunError> {
    let mut predicate: Option<Expr> = None;
    for index in 0..cols.len() {
        let col_i = col_expr(schema, cols[index])?;
        let lit_i = tie_literal(&col_i.data_type(), &values[index])?;
        let mut term = compare(col_i, order, lit_i);
        for earlier in 0..index {
            let col_e = col_expr(schema, cols[earlier])?;
            let lit_e = tie_literal(&col_e.data_type(), &values[earlier])?;
            term = and_expr(eq_expr(col_e, lit_e), term);
        }
        predicate = Some(match predicate {
            None => term,
            Some(previous) => or_expr(previous, term),
        });
    }
    Ok(predicate.unwrap_or(Expr::Literal(ScalarValue::Bool(false))))
}

/// The raw text of `"<field>":<token>` in compact cursor JSON.
fn json_field_token<'a>(json: &'a [u8], field: &str) -> Result<&'a str, RunError> {
    let key = format!("\"{field}\":");
    let key = key.as_bytes();
    let Some(start) = json.windows(key.len()).position(|window| window == key) else {
        return Err(invalid_cursor());
    };
    let rest = &json[start + key.len()..];
    let end = rest
        .iter()
        .position(|byte| *byte == b',' || *byte == b'}')
        .ok_or_else(invalid_cursor)?;
    let token = &rest[..end];
    if token.is_empty()
        || !token
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
    {
        return Err(invalid_cursor());
    }
    std::str::from_utf8(token).map_err(|_| invalid_cursor())
}

fn is_float_column(column: &str) -> bool {
    matches!(column, "cost_total" | "error_rate" | "traces_per_session")
}

fn take_rows(batches: Vec<Batch>, limit: usize) -> Result<Vec<Batch>, RunError> {
    let mut kept = Vec::new();
    let mut remaining = limit;
    for batch in batches {
        if remaining == 0 {
            break;
        }
        let n = batch.num_rows();
        if n == 0 {
            continue;
        }
        if n <= remaining {
            remaining -= n;
            kept.push(batch);
        } else {
            let sliced = match batch.selection() {
                Selection::All => batch.slice(0, remaining),
                Selection::Mask(bm) => {
                    let indices: Vec<u32> =
                        bm.iter_ones().take(remaining).map(|i| i as u32).collect();
                    batch.with_selection(Selection::Indices(indices))
                }
                Selection::Indices(idx) => {
                    batch.with_selection(Selection::Indices(idx[..remaining].to_vec()))
                }
            }
            .map_err(|e| internal(format!("slicing cursor batch failed: {e}")))?;
            kept.push(sliced);
            remaining = 0;
        }
    }
    Ok(kept)
}

fn last_row(batches: &[Batch]) -> Option<(&Batch, usize)> {
    let batch = batches.iter().rev().find(|batch| batch.num_rows() > 0)?;
    Some((batch, batch.num_rows() - 1))
}

fn invalid_cursor() -> RunError {
    RunError::InvalidRequest("invalid cursor".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{
        Bitmap, CancelToken, Column, DataType, Field, FlatColumn, FlatData, Schema, Selection,
        Utf8Data,
    };
    use cetra_query::{EvalContext, eval_filter};

    use super::*;

    #[test]
    fn cost_cursor_keeps_float_bits_and_rejects_oversize_or_nonfinite() {
        let cost = f64::from_bits(0x3f7f_559b_3d07_c84c);
        let cursor = Cursor {
            list: "users".to_owned(),
            sort: "cost_total".to_owned(),
            order: SortOrder::Asc,
            value: CursorValue::Float(cost),
            tie: vec!["user-secret-42".to_owned()],
        };
        let encoded = cursor.encode();
        let decoded = decode(&encoded, "users", "cost_total", SortOrder::Asc).expect("cursor");
        let CursorValue::Float(value) = decoded.value else {
            panic!("cost cursor decoded as a non-float");
        };
        assert_eq!(value.to_bits(), cost.to_bits());

        // Privacy-safe Debug never prints tie contents.
        let dbg = format!("{decoded:?}");
        assert!(dbg.contains("tie_len: 1"));
        assert!(!dbg.contains("user-secret-42"));

        // Wrong list/sort/order error message matches oracle.
        match decode(&encoded, "traces", "cost_total", SortOrder::Asc) {
            Err(RunError::InvalidRequest(msg)) => {
                assert_eq!(msg, "the cursor belongs to another list or sort order");
            }
            other => panic!("unexpected {other:?}"),
        }

        // Oversize cursor rejected.
        let too_big = "a".repeat(MAX_CURSOR_BYTES + 1);
        assert!(matches!(
            decode(&too_big, "users", "cost_total", SortOrder::Asc),
            Err(RunError::InvalidRequest(_))
        ));

        // Non-finite float rejected (1e500 overflows f64 to inf).
        let inf_json = br#"{"l":"users","s":"cost_total","o":"asc","v":1e500,"t":["u1"]}"#;
        let inf_hex = hex::encode(inf_json);
        assert!(matches!(
            decode(&inf_hex, "users", "cost_total", SortOrder::Asc),
            Err(RunError::InvalidRequest(_))
        ));
    }

    #[test]
    fn multi_tie_after_expr_and_assemble_with_selection() {
        let schema = Schema::try_new_arc(vec![
            Field::new("start_time", DataType::timestamp_utc(), true),
            Field::new("span_id", DataType::Utf8, false),
            Field::new("trace_id", DataType::Utf8, false),
        ])
        .expect("schema");

        // Rows in DESC order (NULLS LAST): (200, s2, t1), [masked out], (200, s1, t2), (200, s1, t1), (NULL, s0, t0)
        let batch = Batch::try_new(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![200, 999, 200, 200, 0],
                            utc: true,
                        },
                        Some({
                            let mut bm = Bitmap::new_ones(5);
                            let _ = bm.set(4, false);
                            bm
                        }),
                    )
                    .expect("ts"),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["s2", "sx", "s1", "s1", "s0"]).expect("s"),
                        ),
                        None,
                    )
                    .expect("span_id"),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["t1", "tx", "t2", "t1", "t0"]).expect("t"),
                        ),
                        None,
                    )
                    .expect("trace_id"),
                ),
            ],
            5,
            Selection::Indices(vec![0, 2, 3, 4]),
        )
        .expect("batch");

        // Limit 2 on 4 active rows -> keeps rows 0 and 2, cursor is row 2 = (200, s1, t2).
        let page = assemble(
            vec![batch.clone()],
            2,
            "spans",
            "start_time",
            SortOrder::Desc,
            &["span_id", "trace_id"],
        )
        .expect("assemble");
        assert_eq!(page.batches.iter().map(Batch::num_rows).sum::<usize>(), 2);
        let tok = page.next_cursor.expect("next_cursor");
        let cur = decode(&tok, "spans", "start_time", SortOrder::Desc).expect("decode");
        assert_eq!(cur.value, CursorValue::Int(200));
        assert_eq!(cur.tie, vec!["s1".to_owned(), "t2".to_owned()]);

        let pred = after_expr(&cur, &schema, "start_time", &["span_id", "trace_id"]).expect("pred");
        let eval_ctx = EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        };
        let mask = eval_filter(&pred, &batch, &eval_ctx, None).expect("eval");
        // Active indices: 0 -> false, 2 -> false, 3 -> true (200, s1, t1 < t2), 4 -> true (NULL)
        assert!(!mask.get(0));
        assert!(!mask.get(2));
        assert!(mask.get(3));
        assert!(mask.get(4));

        // SortKey NULLS LAST in both directions.
        let keys = order_by(
            &schema,
            "start_time",
            &["span_id", "trace_id"],
            SortOrder::Desc,
        )
        .expect("order_by");
        assert!(keys.iter().all(|k| k.descending && !k.nulls_first));
    }
}
