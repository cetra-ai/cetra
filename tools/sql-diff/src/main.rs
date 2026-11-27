#![forbid(unsafe_code)]

//! Format-agnostic CLI runner for DuckDB differential testing against `cetra-query`.
//!
//! Reads a JSON bundle (`{tables, queries}`) from a file argument or stdin, constructs
//! in-memory `BatchSource` tables, runs each query through `cetra_query::sql::sql_json`,
//! asserts zero memory/spill leaks after every query, and emits one JSONL record per query
//! to stdout.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::env;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::process::ExitCode;
use std::sync::Arc;

use cetra_column::{
    Batch, BinaryData, Bitmap, CancelToken, Column, DataType, Field, FlatColumn, FlatData,
    GlobalPool, Interval, MAX_NESTING_DEPTH, MapData, ScalarValue, Schema, Utf8Data,
};
use cetra_query::QueryError;
use cetra_query::exec::{
    BatchSource, DEFAULT_BATCH_SIZE, ExecContext, ExecMetrics, SpillManager, WorkerPool,
};
use cetra_query::ir::EvalContext;
use cetra_query::sql::{SqlCatalog, SqlSources, sql_json};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const FIXED_NOW_NS: i64 = 1_767_225_600_000_000_000;
const SPILL_CAP_BYTES: u64 = 64 * 1024 * 1024;
const UNIX_EPOCH_CE_DAYS: i32 = 719_163;

fn default_true() -> bool {
    true
}

fn default_limit() -> usize {
    1000
}

fn default_memory_cap() -> usize {
    32 * 1024 * 1024
}

fn default_target_partitions() -> usize {
    1
}

#[derive(Deserialize)]
struct Bundle {
    tables: Vec<TableSpec>,
    queries: Vec<QuerySpec>,
}

#[derive(Deserialize)]
struct TableSpec {
    name: String,
    columns: Vec<ColumnSpec>,
    rows: Vec<Vec<Value>>,
}

#[derive(Deserialize)]
struct ColumnSpec {
    name: String,
    #[serde(rename = "type")]
    col_type: String,
    #[serde(default = "default_true")]
    nullable: bool,
}

#[derive(Deserialize)]
struct QuerySpec {
    id: String,
    sql: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_memory_cap")]
    memory_cap_bytes: usize,
    #[serde(default = "default_target_partitions")]
    target_partitions: usize,
}

#[derive(Serialize)]
struct OutputColumn<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    col_type: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum QueryOutput<'a> {
    Ok {
        id: &'a str,
        columns: Vec<OutputColumn<'a>>,
        rows: Vec<Map<String, Value>>,
        truncated: bool,
    },
    Error {
        id: &'a str,
        kind: &'static str,
        message: String,
    },
    Leak {
        id: &'a str,
        reserved_bytes: usize,
        spilled_bytes: u64,
        outcome: &'static str,
    },
}

struct DecodedTable {
    name: String,
    schema: Arc<Schema>,
    rows: Vec<Vec<ScalarValue>>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "cetra-sql-diff error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let bundle = read_bundle()?;

    let mut decoded_tables = Vec::with_capacity(bundle.tables.len());
    for table in &bundle.tables {
        decoded_tables.push(decode_table(table)?);
    }

    let spill_dir = tempfile::tempdir().map_err(|e| format!("failed to create spill dir: {e}"))?;
    let mut catalogs: HashMap<usize, (SqlCatalog, SqlSources)> = HashMap::new();
    let mut workers_cache: HashMap<usize, Arc<WorkerPool>> = HashMap::new();

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let empty_params = Map::new();

    for query in &bundle.queries {
        let partitions = query.target_partitions.max(1);

        let (catalog, sources) = match catalogs.entry(partitions) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                entry.insert(build_catalog_and_sources(&decoded_tables, partitions)?)
            }
        };

        let workers = match workers_cache.entry(partitions) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                let pool = WorkerPool::new(partitions)
                    .map_err(|e| format!("failed to create worker pool: {e}"))?;
                Arc::clone(entry.insert(Arc::new(pool)))
            }
        };

        let ctx = ExecContext {
            pool: GlobalPool::standalone(query.memory_cap_bytes),
            eval: EvalContext::new(FIXED_NOW_NS, CancelToken::new()),
            spill: Arc::new(SpillManager::with_fixed_cap(
                spill_dir.path().to_path_buf(),
                SPILL_CAP_BYTES,
            )),
            workers,
            batch_size: DEFAULT_BATCH_SIZE,
            target_partitions: partitions,
            metrics: ExecMetrics::default(),
        };

        let exec_res = sql_json(
            catalog,
            sources,
            &ctx,
            &query.sql,
            &empty_params,
            query.limit,
        );
        let reserved = ctx.pool.reserved();
        let spilled = ctx.spill.spilled_bytes();

        if reserved != 0 || spilled != 0 {
            let record = QueryOutput::Leak {
                id: &query.id,
                reserved_bytes: reserved,
                spilled_bytes: spilled,
                outcome: if exec_res.is_ok() { "ok" } else { "error" },
            };
            write_jsonl_line(&mut out, &record)?;
            continue;
        }

        match exec_res {
            Ok(res) => {
                let columns = res
                    .columns
                    .iter()
                    .map(|c| OutputColumn {
                        name: &c.name,
                        col_type: c.type_name(),
                    })
                    .collect();
                let record = QueryOutput::Ok {
                    id: &query.id,
                    columns,
                    rows: res.rows,
                    truncated: res.truncated,
                };
                write_jsonl_line(&mut out, &record)?;
            }
            Err(err) => {
                let record = QueryOutput::Error {
                    id: &query.id,
                    kind: error_kind(&err),
                    message: err.to_string(),
                };
                write_jsonl_line(&mut out, &record)?;
            }
        }
    }

    out.flush()
        .map_err(|e| format!("failed to flush stdout: {e}"))?;
    Ok(())
}

fn read_bundle() -> Result<Bundle, String> {
    let arg_path = env::args().nth(1);
    match arg_path.as_deref() {
        Some(path) if path != "-" => {
            let file =
                File::open(path).map_err(|e| format!("failed to open input file `{path}`: {e}"))?;
            let reader = BufReader::new(file);
            serde_json::from_reader(reader)
                .map_err(|e| format!("failed to parse bundle JSON from `{path}`: {e}"))
        }
        _ => {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin.lock());
            serde_json::from_reader(reader)
                .map_err(|e| format!("failed to parse bundle JSON from stdin: {e}"))
        }
    }
}

fn write_jsonl_line<T: Serialize>(out: &mut impl Write, record: &T) -> Result<(), String> {
    serde_json::to_writer(&mut *out, record)
        .map_err(|e| format!("failed to serialize output record: {e}"))?;
    writeln!(out).map_err(|e| format!("failed to write newline to stdout: {e}"))
}

fn error_kind(err: &QueryError) -> &'static str {
    match err {
        QueryError::Invalid(_) => "invalid",
        QueryError::InvalidRequest(_) => "invalid_request",
        QueryError::Failed(_) => "query_failed",
        QueryError::TooLarge => "too_large",
        QueryError::Cancelled => "cancelled",
        QueryError::Busy => "busy",
        QueryError::Internal(_)
        | QueryError::CorruptData(_)
        | QueryError::StorageIo(_)
        | QueryError::Column(_) => "internal",
    }
}

fn decode_table(spec: &TableSpec) -> Result<DecodedTable, String> {
    let mut fields = Vec::with_capacity(spec.columns.len());
    for col in &spec.columns {
        let dt = parse_data_type(&col.col_type)?;
        fields.push(Field::new(col.name.clone(), dt, col.nullable));
    }
    let schema = Schema::try_new_arc(fields)
        .map_err(|e| format!("invalid schema for table `{}`: {e}", spec.name))?;

    let mut rows = Vec::with_capacity(spec.rows.len());
    for (row_idx, raw_row) in spec.rows.iter().enumerate() {
        if raw_row.len() != schema.len() {
            return Err(format!(
                "table `{}` row {row_idx} has {} values, expected {}",
                spec.name,
                raw_row.len(),
                schema.len()
            ));
        }
        let mut decoded_row = Vec::with_capacity(schema.len());
        for (col_idx, (val, field)) in raw_row.iter().zip(schema.fields()).enumerate() {
            let scalar = decode_scalar(val, &field.data_type).map_err(|e| {
                format!(
                    "table `{}` row {row_idx} col {col_idx} (`{}`): {e}",
                    spec.name, field.name
                )
            })?;
            if !field.nullable && scalar.is_null() {
                return Err(format!(
                    "table `{}` row {row_idx} col `{}` is NOT NULL but received null",
                    spec.name, field.name
                ));
            }
            decoded_row.push(scalar);
        }
        rows.push(decoded_row);
    }

    Ok(DecodedTable {
        name: spec.name.clone(),
        schema,
        rows,
    })
}

fn build_catalog_and_sources(
    tables: &[DecodedTable],
    target_partitions: usize,
) -> Result<(SqlCatalog, SqlSources), String> {
    let mut catalog = SqlCatalog::new(None, None);
    for table in tables {
        let source = if table.rows.is_empty() {
            Arc::new(BatchSource::empty(Arc::clone(&table.schema)))
        } else {
            let num_batches = target_partitions.min(table.rows.len()).max(1);
            let base = table.rows.len() / num_batches;
            let rem = table.rows.len() % num_batches;
            let mut batches = Vec::with_capacity(num_batches);
            let mut start = 0usize;
            for i in 0..num_batches {
                let len = base + usize::from(i < rem);
                let slice = &table.rows[start..start + len];
                batches.push(build_batch(Arc::clone(&table.schema), slice)?);
                start += len;
            }
            Arc::new(
                BatchSource::new(Arc::clone(&table.schema), batches)
                    .map_err(|e| format!("failed to create BatchSource `{}`: {e}", table.name))?,
            )
        };
        catalog
            .register_batch_source(&table.name, source)
            .map_err(|e| format!("failed to register table `{}`: {e}", table.name))?;
    }
    let mut sources = SqlSources::new();
    sources.register_catalog_batches(&catalog);
    Ok((catalog, sources))
}

fn build_batch(schema: Arc<Schema>, rows: &[Vec<ScalarValue>]) -> Result<Batch, String> {
    let num_rows = rows.len();
    if num_rows == 0 {
        return Ok(Batch::empty(schema));
    }
    let mut columns = Vec::with_capacity(schema.len());
    for (col_idx, field) in schema.fields().iter().enumerate() {
        let mut col_scalars = Vec::with_capacity(num_rows);
        for row in rows {
            col_scalars.push(row[col_idx].clone());
        }
        columns.push(build_flat_column(&field.data_type, &col_scalars)?);
    }
    Batch::try_new_all(schema, columns, num_rows).map_err(|e| format!("failed to build batch: {e}"))
}

fn parse_data_type(raw: &str) -> Result<DataType, String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_suffix("[]") {
        let child = parse_data_type(inner)?;
        let dt = DataType::List(Box::new(child));
        dt.validate(MAX_NESTING_DEPTH)
            .map_err(|e| format!("invalid list type `{raw}`: {e}"))?;
        return Ok(dt);
    }

    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("LIST(") && upper.ends_with(')') {
        let inner_raw = &trimmed[5..trimmed.len() - 1];
        let child = parse_data_type(inner_raw)?;
        let dt = DataType::List(Box::new(child));
        dt.validate(MAX_NESTING_DEPTH)
            .map_err(|e| format!("invalid list type `{raw}`: {e}"))?;
        return Ok(dt);
    }

    if let Some(inner) = upper
        .strip_prefix("DECIMAL(")
        .or_else(|| upper.strip_prefix("NUMERIC("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let mut parts = inner.split(',');
        let p_str = parts
            .next()
            .ok_or_else(|| format!("missing decimal precision in `{raw}`"))?;
        let s_str = parts
            .next()
            .ok_or_else(|| format!("missing decimal scale in `{raw}`"))?;
        if parts.next().is_some() {
            return Err(format!("too many arguments in decimal type `{raw}`"));
        }
        let precision = p_str
            .trim()
            .parse::<u8>()
            .map_err(|_| format!("invalid decimal precision in `{raw}`"))?;
        let scale = s_str
            .trim()
            .parse::<i8>()
            .map_err(|_| format!("invalid decimal scale in `{raw}`"))?;
        let dt = DataType::Decimal128 { precision, scale };
        dt.validate(MAX_NESTING_DEPTH)
            .map_err(|e| format!("invalid decimal type `{raw}`: {e}"))?;
        return Ok(dt);
    }

    if let Some(inner) = upper
        .strip_prefix("MAP(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let mut parts = inner.split(',');
        let key_ty = parts
            .next()
            .ok_or_else(|| format!("missing map key type in `{raw}`"))?
            .trim();
        let val_ty = parts
            .next()
            .ok_or_else(|| format!("missing map value type in `{raw}`"))?
            .trim();
        if parts.next().is_some() {
            return Err(format!("too many arguments in map type `{raw}`"));
        }
        let is_utf8 = |ty: &str| matches!(ty, "UTF8" | "VARCHAR" | "TEXT" | "STRING");
        if !is_utf8(key_ty) || !is_utf8(val_ty) {
            return Err(format!(
                "unsupported map type `{raw}`: only UTF-8 string keys and values are supported"
            ));
        }
        return Ok(DataType::MapUtf8);
    }

    match upper.as_str() {
        "NULL" => Ok(DataType::Null),
        "BOOL" | "BOOLEAN" => Ok(DataType::Bool),
        "I64" | "INT64" | "BIGINT" | "INT" | "INTEGER" => Ok(DataType::I64),
        "F64" | "FLOAT64" | "DOUBLE" | "FLOAT" => Ok(DataType::F64),
        "TIMESTAMP" => Ok(DataType::TimestampNs { utc: false }),
        "TIMESTAMPTZ" => Ok(DataType::TimestampNs { utc: true }),
        "DATE" => Ok(DataType::Date),
        "INTERVAL" => Ok(DataType::Interval),
        "UTF8" | "VARCHAR" | "TEXT" | "STRING" => Ok(DataType::Utf8),
        "BINARY" | "BYTEA" | "BLOB" => Ok(DataType::Binary),
        "FIXEDBYTES8" => Ok(DataType::FixedBytes8),
        "FIXEDBYTES16" => Ok(DataType::FixedBytes16),
        "MAP_UTF8" => Ok(DataType::MapUtf8),
        _ => Err(format!("unsupported column type `{raw}`")),
    }
}

fn decode_scalar(val: &Value, dt: &DataType) -> Result<ScalarValue, String> {
    if val.is_null() {
        return Ok(ScalarValue::Null(dt.clone()));
    }

    match dt {
        DataType::Null => Err("expected null for NULL column".to_owned()),
        DataType::Bool => val
            .as_bool()
            .map(ScalarValue::Bool)
            .ok_or_else(|| "expected boolean".to_owned()),
        DataType::I64 => match val {
            Value::Number(n) => n
                .as_i64()
                .map(ScalarValue::I64)
                .ok_or_else(|| "integer out of i64 range".to_owned()),
            Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map(ScalarValue::I64)
                .map_err(|_| "invalid i64 string".to_owned()),
            _ => Err("expected integer or i64 string".to_owned()),
        },
        DataType::F64 => match val {
            Value::Number(n) => n
                .as_f64()
                .map(ScalarValue::F64)
                .ok_or_else(|| "invalid f64 number".to_owned()),
            Value::String(s) => {
                let t = s.trim();
                let parsed = match t.to_ascii_lowercase().as_str() {
                    "nan" => f64::NAN,
                    "infinity" | "+infinity" | "inf" | "+inf" => f64::INFINITY,
                    "-infinity" | "-inf" => f64::NEG_INFINITY,
                    _ => t
                        .parse::<f64>()
                        .map_err(|_| "invalid f64 string".to_owned())?,
                };
                Ok(ScalarValue::F64(parsed))
            }
            _ => Err("expected float number or string".to_owned()),
        },
        DataType::Decimal128 { precision, scale } => {
            let unscaled = match val {
                Value::String(s) => parse_decimal_str(s, *precision, *scale)?,
                Value::Number(n) => {
                    let text = n.to_string();
                    parse_decimal_str(&text, *precision, *scale)?
                }
                _ => return Err("expected decimal string or number".to_owned()),
            };
            Ok(ScalarValue::Decimal128 {
                value: unscaled,
                precision: *precision,
                scale: *scale,
            })
        }
        DataType::TimestampNs { utc } => {
            let ns = match val {
                Value::Number(n) => n
                    .as_i64()
                    .ok_or_else(|| "timestamp nanos out of i64 range".to_owned())?,
                Value::String(s) => parse_timestamp_ns(s)?,
                _ => return Err("expected timestamp integer nanos or ISO string".to_owned()),
            };
            Ok(ScalarValue::TimestampNs {
                value: ns,
                utc: *utc,
            })
        }
        DataType::Date => {
            let days = match val {
                Value::Number(n) => {
                    let raw = n
                        .as_i64()
                        .ok_or_else(|| "date days out of integer range".to_owned())?;
                    i32::try_from(raw).map_err(|_| "date days out of i32 range".to_owned())?
                }
                Value::String(s) => {
                    let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                        .map_err(|_| "invalid date string (expected YYYY-MM-DD)".to_owned())?;
                    d.num_days_from_ce() - UNIX_EPOCH_CE_DAYS
                }
                _ => return Err("expected date days integer or YYYY-MM-DD string".to_owned()),
            };
            Ok(ScalarValue::Date(days))
        }
        DataType::Interval => {
            let Some(obj) = val.as_object() else {
                return Err("expected interval object {months, days, nanos}".to_owned());
            };
            for key in obj.keys() {
                if !matches!(key.as_str(), "months" | "days" | "nanos") {
                    return Err(
                        "unknown field in interval object (expected months, days, nanos)"
                            .to_owned(),
                    );
                }
            }
            let months = match obj.get("months") {
                None | Some(Value::Null) => 0i32,
                Some(Value::Number(n)) => {
                    let v = n
                        .as_i64()
                        .ok_or_else(|| "invalid interval months".to_owned())?;
                    i32::try_from(v).map_err(|_| "interval months out of i32 range".to_owned())?
                }
                _ => return Err("interval months must be integer".to_owned()),
            };
            let days = match obj.get("days") {
                None | Some(Value::Null) => 0i32,
                Some(Value::Number(n)) => {
                    let v = n
                        .as_i64()
                        .ok_or_else(|| "invalid interval days".to_owned())?;
                    i32::try_from(v).map_err(|_| "interval days out of i32 range".to_owned())?
                }
                _ => return Err("interval days must be integer".to_owned()),
            };
            let nanos = match obj.get("nanos") {
                None | Some(Value::Null) => 0i64,
                Some(Value::Number(n)) => n
                    .as_i64()
                    .ok_or_else(|| "interval nanos out of i64 range".to_owned())?,
                Some(Value::String(s)) => s
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "invalid interval nanos string".to_owned())?,
                _ => return Err("interval nanos must be integer".to_owned()),
            };
            Ok(ScalarValue::Interval(Interval {
                months,
                days,
                nanos,
            }))
        }
        DataType::Utf8 => val
            .as_str()
            .map(|s| ScalarValue::Utf8(s.to_owned()))
            .ok_or_else(|| "expected string".to_owned()),
        DataType::Binary => {
            let s = val
                .as_str()
                .ok_or_else(|| "expected hex string for binary".to_owned())?;
            let bytes =
                hex::decode(s.trim()).map_err(|_| "invalid hex binary string".to_owned())?;
            Ok(ScalarValue::Binary(bytes))
        }
        DataType::FixedBytes8 => {
            let s = val
                .as_str()
                .ok_or_else(|| "expected hex string for fixedbytes8".to_owned())?;
            let bytes = hex::decode(s.trim()).map_err(|_| "invalid hex fixedbytes8".to_owned())?;
            let arr: [u8; 8] = bytes
                .try_into()
                .map_err(|_| "fixedbytes8 requires exactly 8 bytes".to_owned())?;
            Ok(ScalarValue::FixedBytes8(arr))
        }
        DataType::FixedBytes16 => {
            let s = val
                .as_str()
                .ok_or_else(|| "expected hex string for fixedbytes16".to_owned())?;
            let bytes = hex::decode(s.trim()).map_err(|_| "invalid hex fixedbytes16".to_owned())?;
            let arr: [u8; 16] = bytes
                .try_into()
                .map_err(|_| "fixedbytes16 requires exactly 16 bytes".to_owned())?;
            Ok(ScalarValue::FixedBytes16(arr))
        }
        DataType::List(inner) => {
            let arr = val
                .as_array()
                .ok_or_else(|| "expected JSON array for list".to_owned())?;
            let mut values = Vec::with_capacity(arr.len());
            for item in arr {
                values.push(decode_scalar(item, inner)?);
            }
            Ok(ScalarValue::List {
                item_type: (**inner).clone(),
                values,
            })
        }
        DataType::MapUtf8 => match val {
            Value::Object(map) => {
                let mut entries = Vec::with_capacity(map.len());
                for (k, v) in map {
                    let mapped = match v {
                        Value::Null => None,
                        Value::String(s) => Some(s.clone()),
                        _ => return Err("map value must be string or null".to_owned()),
                    };
                    entries.push((k.clone(), mapped));
                }
                Ok(ScalarValue::MapUtf8(entries))
            }
            Value::Array(pairs) => {
                let mut entries = Vec::with_capacity(pairs.len());
                for pair in pairs {
                    let items = pair
                        .as_array()
                        .filter(|p| p.len() == 2)
                        .ok_or_else(|| "map pair must be [key, value]".to_owned())?;
                    let key = items[0]
                        .as_str()
                        .ok_or_else(|| "map key must be string".to_owned())?;
                    let mapped = match &items[1] {
                        Value::Null => None,
                        Value::String(s) => Some(s.clone()),
                        _ => return Err("map value must be string or null".to_owned()),
                    };
                    entries.push((key.to_owned(), mapped));
                }
                Ok(ScalarValue::MapUtf8(entries))
            }
            _ => Err("expected JSON object or array of pairs for map".to_owned()),
        },
        DataType::Events | DataType::Links => Err("unsupported input data type".to_owned()),
    }
}

fn parse_decimal_str(raw: &str, precision: u8, scale: i8) -> Result<i128, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty decimal string".to_owned());
    }
    let (neg, rest) = if let Some(after) = trimmed.strip_prefix('-') {
        (true, after)
    } else if let Some(after) = trimmed.strip_prefix('+') {
        (false, after)
    } else {
        (false, trimmed)
    };

    let (mantissa, exp_opt) = match rest.find(['e', 'E']) {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    let exp: i64 = match exp_opt {
        Some(exp_str) => {
            if exp_str.is_empty() {
                return Err("empty decimal exponent".to_owned());
            }
            exp_str
                .parse::<i64>()
                .map_err(|_| "invalid decimal exponent".to_owned())?
        }
        None => 0,
    };

    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => {
            if f.contains('.') {
                return Err("invalid decimal: multiple decimal points".to_owned());
            }
            (i, f)
        }
        None => (mantissa, ""),
    };

    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("invalid decimal digits".to_owned());
    }

    let mut all_digits = Vec::with_capacity(int_part.len() + frac_part.len());
    all_digits.extend_from_slice(int_part.as_bytes());
    all_digits.extend_from_slice(frac_part.as_bytes());

    let Some(first_nz) = all_digits.iter().position(|&b| b != b'0') else {
        return Ok(0);
    };
    let Some(last_nz) = all_digits.iter().rposition(|&b| b != b'0') else {
        return Ok(0);
    };
    let sig_digits = &all_digits[first_nz..=last_nz];
    let trailing_zeros = all_digits.len() - 1 - last_nz;

    let frac_len = i128::try_from(frac_part.len())
        .map_err(|_| "decimal fractional length out of range".to_owned())?;
    let trail_len = i128::try_from(trailing_zeros)
        .map_err(|_| "decimal trailing zeros out of range".to_owned())?;
    let shift = i128::from(exp)
        .checked_add(i128::from(scale))
        .and_then(|v| v.checked_sub(frac_len))
        .and_then(|v| v.checked_add(trail_len))
        .ok_or_else(|| "decimal exponent out of range".to_owned())?;

    if shift < 0 {
        return Err("decimal value not representable at column scale".to_owned());
    }
    let shift_u32 = u32::try_from(shift)
        .map_err(|_| format!("decimal value exceeds precision {precision} bounds"))?;
    if shift_u32 > u32::from(precision) || sig_digits.len() > usize::from(precision) {
        return Err(format!(
            "decimal value exceeds precision {precision} bounds"
        ));
    }

    let mut acc: i128 = 0;
    for &b in sig_digits {
        let d = i128::from(b - b'0');
        acc = acc
            .checked_mul(10)
            .and_then(|v| v.checked_add(d))
            .ok_or_else(|| "decimal value overflows i128".to_owned())?;
    }
    let factor = 10_i128
        .checked_pow(shift_u32)
        .ok_or_else(|| "decimal value overflows i128".to_owned())?;
    acc = acc
        .checked_mul(factor)
        .ok_or_else(|| "decimal value overflows i128".to_owned())?;

    let max_abs = 10_i128
        .checked_pow(u32::from(precision))
        .and_then(|v| v.checked_sub(1))
        .ok_or_else(|| "decimal precision out of range".to_owned())?;
    if acc > max_abs {
        return Err(format!(
            "decimal value exceeds precision {precision} bounds"
        ));
    }
    Ok(if neg { -acc } else { acc })
}

fn parse_timestamp_ns(raw: &str) -> Result<i64, String> {
    let trimmed = raw.trim();
    if let Ok(ns) = trimmed.parse::<i64>() {
        return Ok(ns);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return dt
            .timestamp_nanos_opt()
            .ok_or_else(|| "timestamp out of nanosecond i64 range".to_owned());
    }
    if trimmed.contains(' ') {
        let with_t = trimmed.replacen(' ', "T", 1);
        if let Ok(dt) = DateTime::parse_from_rfc3339(&with_t) {
            return dt
                .timestamp_nanos_opt()
                .ok_or_else(|| "timestamp out of nanosecond i64 range".to_owned());
        }
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            return ndt
                .and_utc()
                .timestamp_nanos_opt()
                .ok_or_else(|| "timestamp out of nanosecond i64 range".to_owned());
        }
    }
    Err("invalid timestamp string".to_owned())
}

fn build_flat_column(dt: &DataType, scalars: &[ScalarValue]) -> Result<Column, String> {
    let len = scalars.len();
    if len == 0 {
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }

    let mut valid_bools = Vec::with_capacity(len);
    for sv in scalars {
        valid_bools.push(!sv.is_null());
    }
    let valid_bm = Bitmap::from_bools(&valid_bools);
    let nulls = if valid_bm.all_ones() {
        None
    } else {
        Some(valid_bm)
    };

    let data = match dt {
        DataType::Null => FlatData::Null(len),
        DataType::Bool => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::Bool(b) => vals.push(*b),
                    ScalarValue::Null(_) => vals.push(false),
                    _ => return Err("type mismatch building bool column".to_owned()),
                }
            }
            FlatData::Bool(Bitmap::from_bools(&vals))
        }
        DataType::I64 => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::I64(v) => vals.push(*v),
                    ScalarValue::Null(_) => vals.push(0),
                    _ => return Err("type mismatch building i64 column".to_owned()),
                }
            }
            FlatData::I64(vals)
        }
        DataType::F64 => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::F64(v) => vals.push(*v),
                    ScalarValue::Null(_) => vals.push(0.0),
                    _ => return Err("type mismatch building f64 column".to_owned()),
                }
            }
            FlatData::F64(vals)
        }
        DataType::Decimal128 { precision, scale } => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::Decimal128 { value, .. } => vals.push(*value),
                    ScalarValue::Null(_) => vals.push(0),
                    _ => return Err("type mismatch building decimal column".to_owned()),
                }
            }
            FlatData::Decimal128 {
                values: vals,
                precision: *precision,
                scale: *scale,
            }
        }
        DataType::TimestampNs { utc } => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::TimestampNs { value, .. } => vals.push(*value),
                    ScalarValue::Null(_) => vals.push(0),
                    _ => return Err("type mismatch building timestamp column".to_owned()),
                }
            }
            FlatData::TimestampNs {
                values: vals,
                utc: *utc,
            }
        }
        DataType::Date => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::Date(v) => vals.push(*v),
                    ScalarValue::Null(_) => vals.push(0),
                    _ => return Err("type mismatch building date column".to_owned()),
                }
            }
            FlatData::Date(vals)
        }
        DataType::Interval => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::Interval(v) => vals.push(*v),
                    ScalarValue::Null(_) => vals.push(Interval::default()),
                    _ => return Err("type mismatch building interval column".to_owned()),
                }
            }
            FlatData::Interval(vals)
        }
        DataType::Utf8 => {
            let mut slices: Vec<&str> = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::Utf8(s) => slices.push(s.as_str()),
                    ScalarValue::Null(_) => slices.push(""),
                    _ => return Err("type mismatch building utf8 column".to_owned()),
                }
            }
            let utf8 = Utf8Data::try_from_slices(slices)
                .map_err(|e| format!("failed to build utf8 column: {e}"))?;
            FlatData::Utf8(utf8)
        }
        DataType::Binary => {
            let mut slices: Vec<&[u8]> = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::Binary(b) => slices.push(b.as_slice()),
                    ScalarValue::Null(_) => slices.push(&[]),
                    _ => return Err("type mismatch building binary column".to_owned()),
                }
            }
            let bin = BinaryData::try_from_slices(slices)
                .map_err(|e| format!("failed to build binary column: {e}"))?;
            FlatData::Binary(bin)
        }
        DataType::FixedBytes8 => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::FixedBytes8(v) => vals.push(*v),
                    ScalarValue::Null(_) => vals.push([0u8; 8]),
                    _ => return Err("type mismatch building fixedbytes8 column".to_owned()),
                }
            }
            FlatData::FixedBytes8(vals)
        }
        DataType::FixedBytes16 => {
            let mut vals = Vec::with_capacity(len);
            for sv in scalars {
                match sv {
                    ScalarValue::FixedBytes16(v) => vals.push(*v),
                    ScalarValue::Null(_) => vals.push([0u8; 16]),
                    _ => return Err("type mismatch building fixedbytes16 column".to_owned()),
                }
            }
            FlatData::FixedBytes16(vals)
        }
        DataType::List(inner) => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut flat_elems: Vec<ScalarValue> = Vec::new();
            for sv in scalars {
                match sv {
                    ScalarValue::List { values, .. } => {
                        flat_elems.extend(values.iter().cloned());
                    }
                    ScalarValue::Null(_) => {}
                    _ => return Err("type mismatch building list column".to_owned()),
                }
                let end = u32::try_from(flat_elems.len())
                    .map_err(|_| "list element count exceeds u32::MAX".to_owned())?;
                offsets.push(end);
            }
            let child_col = build_flat_column(inner, &flat_elems)?;
            FlatData::List {
                item_type: (**inner).clone(),
                offsets,
                values: Box::new(child_col),
            }
        }
        DataType::MapUtf8 => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut key_slices: Vec<&str> = Vec::new();
            let mut val_slices: Vec<&str> = Vec::new();
            let mut val_valid: Vec<bool> = Vec::new();
            for sv in scalars {
                match sv {
                    ScalarValue::MapUtf8(entries) => {
                        for (k, v) in entries {
                            key_slices.push(k.as_str());
                            match v {
                                Some(s) => {
                                    val_slices.push(s.as_str());
                                    val_valid.push(true);
                                }
                                None => {
                                    val_slices.push("");
                                    val_valid.push(false);
                                }
                            }
                        }
                    }
                    ScalarValue::Null(_) => {}
                    _ => return Err("type mismatch building map column".to_owned()),
                }
                let end = u32::try_from(key_slices.len())
                    .map_err(|_| "map entry count exceeds u32::MAX".to_owned())?;
                offsets.push(end);
            }
            let keys = Utf8Data::try_from_slices(key_slices)
                .map_err(|e| format!("failed to build map keys: {e}"))?;
            let values = Utf8Data::try_from_slices(val_slices)
                .map_err(|e| format!("failed to build map values: {e}"))?;
            let value_nulls = if val_valid.is_empty() {
                None
            } else {
                Some(Bitmap::from_bools(&val_valid))
            };
            let map_data = MapData::try_new(offsets, keys, values, value_nulls)
                .map_err(|e| format!("failed to build map data: {e}"))?;
            FlatData::MapUtf8(Box::new(map_data))
        }
        DataType::Events | DataType::Links => {
            return Err("unsupported flat column data type".to_owned());
        }
    };

    let flat = FlatColumn::try_new(data, nulls)
        .map_err(|e| format!("failed to construct FlatColumn: {e}"))?;
    Ok(Column::Flat(flat))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_data_type_validates_map_types() {
        assert_eq!(
            parse_data_type("MAP(VARCHAR, VARCHAR)"),
            Ok(DataType::MapUtf8)
        );
        assert_eq!(parse_data_type("MAP(TEXT, STRING)"), Ok(DataType::MapUtf8));
        assert_eq!(parse_data_type("MAP(UTF8, UTF8)"), Ok(DataType::MapUtf8));
        assert!(parse_data_type("MAP(INT, INT)").is_err());
        assert!(parse_data_type("MAP(VARCHAR, BIGINT)").is_err());
        assert!(parse_data_type("MAP(VARCHAR)").is_err());
        assert!(parse_data_type("MAP()").is_err());
    }

    #[test]
    fn decode_null_column_rejects_non_null() {
        assert_eq!(
            decode_scalar(&Value::Null, &DataType::Null),
            Ok(ScalarValue::Null(DataType::Null))
        );
        assert!(decode_scalar(&json!(42), &DataType::Null).is_err());
        assert!(decode_scalar(&json!("x"), &DataType::Null).is_err());
    }

    #[test]
    fn parse_decimal_supports_scientific_notation() {
        assert_eq!(parse_decimal_str("1.25e3", 10, 2), Ok(125_000));
        assert_eq!(parse_decimal_str("1.5E-2", 10, 4), Ok(150));
        assert_eq!(parse_decimal_str("-2.5e+1", 10, 2), Ok(-2_500));
        assert_eq!(parse_decimal_str("0e1000000", 10, 2), Ok(0));
        assert_eq!(parse_decimal_str("1.2e3", 5, -2), Ok(12));
        assert!(parse_decimal_str("1e999999", 10, 2).is_err());
        assert!(parse_decimal_str("1.23e-1", 10, 2).is_err());
        assert!(parse_decimal_str("1e", 10, 2).is_err());
    }

    #[test]
    fn decode_interval_rejects_unknown_keys() {
        let ok = decode_scalar(
            &json!({"months": 1, "days": -2, "nanos": 500}),
            &DataType::Interval,
        );
        assert_eq!(
            ok,
            Ok(ScalarValue::Interval(Interval {
                months: 1,
                days: -2,
                nanos: 500,
            }))
        );
        assert!(
            decode_scalar(
                &json!({"months": 1, "days": 0, "micros": 10}),
                &DataType::Interval,
            )
            .is_err()
        );
    }
}
