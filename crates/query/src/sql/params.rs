//! `{name:Type}` parameter scanning, validation, JSON parsing, and usage-site coercion.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use cetra_column::{Batch, CancelToken, DataType, ScalarValue, Schema};
use chrono::{DateTime, Timelike, Utc};
use serde_json::{Map, Number, Value};
use sqlparser::tokenizer::{Span, Token, TokenWithSpan, Word};

use crate::bind::can_cast;
use crate::error::{InvalidQuery, Position, QueryError};
use crate::eval::eval_expr;
use crate::ir::{CastMode, EvalContext, Expr};
use crate::text::position_of;

/// One distinct `{name:Type}` placeholder in a SQL query, in first-appearance order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    /// Parameter name (without braces or type).
    pub name: String,
    /// Declared parameter kind.
    pub kind: ParamKind,
    /// 1-based start position of `{name:Type}` in the query text.
    pub position: Position,
}

/// The five parameter types accepted by `{name:Type}` placeholders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// `String` -> `DataType::Utf8`.
    String,
    /// `Int` -> `DataType::I64`.
    Int,
    /// `Float` -> `DataType::F64`.
    Float,
    /// `Bool` -> `DataType::Bool`.
    Bool,
    /// `Timestamp` -> `DataType::TimestampNs { utc: true }`.
    Timestamp,
}

impl ParamKind {
    /// Case-insensitive parser for `{name:Type}` type names.
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "string" => Some(Self::String),
            "int" => Some(Self::Int),
            "float" => Some(Self::Float),
            "bool" => Some(Self::Bool),
            "timestamp" => Some(Self::Timestamp),
            _ => None,
        }
    }

    /// Column [`DataType`] produced by this parameter kind.
    pub fn data_type(self) -> DataType {
        match self {
            Self::String => DataType::Utf8,
            Self::Int => DataType::I64,
            Self::Float => DataType::F64,
            Self::Bool => DataType::Bool,
            Self::Timestamp => DataType::TimestampNs { utc: true },
        }
    }

    fn expectation(self) -> &'static str {
        match self {
            Self::String => "a string",
            Self::Int => "an integer",
            Self::Float => "a number",
            Self::Bool => "a boolean",
            Self::Timestamp => "an RFC 3339 string or an integer",
        }
    }
}

/// Parsed JSON parameter value bound to a [`Placeholder`].
///
/// Implements a manual privacy-safe [`fmt::Debug`] that logs only `name` and `data_type`
/// (never the parameter value itself, per `AGENTS.md` § Privacy).
#[derive(Clone, PartialEq)]
pub struct SqlParamValue {
    /// Parameter name.
    pub name: String,
    /// Typed scalar value.
    pub value: ScalarValue,
}

impl fmt::Debug for SqlParamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlParamValue")
            .field("name", &self.name)
            .field("data_type", &self.value.data_type().display_name())
            .finish()
    }
}

/// Replaces each `{name:Type}` with `$k` and records distinct placeholders in appearance order.
///
/// A `{` that is not the start of that exact five-token pattern is left for the SQL parser.
/// Raw `$1` and `?` placeholders are rejected here.
pub fn bind_placeholders(
    tokens: Vec<TokenWithSpan>,
) -> Result<(Vec<TokenWithSpan>, Vec<Placeholder>), InvalidQuery> {
    let mut incoming: VecDeque<TokenWithSpan> = tokens.into_iter().collect();
    let mut rewritten = Vec::with_capacity(incoming.len());
    let mut placeholders = Vec::new();
    let mut seen: HashMap<String, Seen> = HashMap::new();
    while let Some(open) = incoming.pop_front() {
        if !matches!(open.token, Token::LBrace) {
            reject_raw_placeholder(&open)?;
            rewritten.push(open);
            continue;
        }
        let Some(name_token) = incoming.pop_front() else {
            rewritten.push(open);
            break;
        };
        let Some(colon) = incoming.pop_front() else {
            rewritten.extend([open, name_token]);
            break;
        };
        let Some(type_token) = incoming.pop_front() else {
            rewritten.extend([open, name_token, colon]);
            break;
        };
        let Some(close) = incoming.pop_front() else {
            rewritten.extend([open, name_token, colon, type_token]);
            break;
        };
        let Some(found) = parameter(&open, &name_token, &colon, &type_token, &close) else {
            incoming.push_front(close);
            incoming.push_front(type_token);
            incoming.push_front(colon);
            incoming.push_front(name_token);
            rewritten.push(open);
            continue;
        };
        let kind = ParamKind::parse(&found.type_name).ok_or_else(|| {
            InvalidQuery::new(
                format!(
                    "unknown parameter type `{}`; use String, Int, Float, Bool or Timestamp",
                    found.type_name
                ),
                Some(found.type_position),
            )
        })?;
        let k = if let Some(previous) = seen.get(&found.name) {
            if previous.kind != kind {
                return Err(InvalidQuery::new(
                    format!("parameter `{}` is declared with two types", found.name),
                    Some(found.type_position),
                ));
            }
            previous.k
        } else {
            let k = placeholders.len() + 1;
            seen.insert(found.name.clone(), Seen { k, kind });
            placeholders.push(Placeholder {
                name: found.name,
                kind,
                position: found.position,
            });
            k
        };
        rewritten.push(TokenWithSpan::new(
            Token::Placeholder(format!("${k}")),
            found.span,
        ));
    }
    Ok((rewritten, placeholders))
}

/// Validates and converts JSON `params` for `placeholders`, in `$k` order.
pub fn parse_param_values(
    placeholders: &[Placeholder],
    params: &Map<String, Value>,
) -> Result<Vec<SqlParamValue>, QueryError> {
    let mut values = Vec::with_capacity(placeholders.len());
    for placeholder in placeholders {
        let Some(value) = params.get(&placeholder.name) else {
            return Err(QueryError::invalid_request(format!(
                "missing value for parameter {}",
                placeholder.name
            )));
        };
        values.push(SqlParamValue {
            name: placeholder.name.clone(),
            value: convert(placeholder, value)?,
        });
    }
    for name in params.keys() {
        if !placeholders
            .iter()
            .any(|placeholder| placeholder.name == *name)
        {
            return Err(QueryError::invalid_request(format!(
                "unknown parameter {name}"
            )));
        }
    }
    Ok(values)
}

/// Coerces a bound parameter value to the usage-site `target` type via strict SQL cast semantics.
pub fn coerce_param_value(
    param: &SqlParamValue,
    target: &DataType,
) -> Result<ScalarValue, QueryError> {
    if param.value.data_type() == *target {
        return Ok(param.value.clone());
    }
    let err = || {
        QueryError::invalid_request(format!(
            "parameter {} does not fit where it is used ({})",
            param.name,
            target.display_name()
        ))
    };
    if !can_cast(&param.value.data_type(), target) {
        return Err(err());
    }
    let schema = Schema::try_new_arc(Vec::new())?;
    let batch = Batch::try_new_all(schema, Vec::new(), 1)?;
    let ctx = EvalContext {
        now_ns: 0,
        cancel: CancelToken::new(),
    };
    let cast_expr = Expr::Cast {
        expr: Box::new(Expr::Literal(param.value.clone())),
        target: target.clone(),
        mode: CastMode::Strict,
    };
    let col = eval_expr(&cast_expr, &batch, &ctx, None).map_err(|_| err())?;
    col.get_scalar(0).map_err(|_| err())
}

struct Seen {
    k: usize,
    kind: ParamKind,
}

struct Found {
    name: String,
    type_name: String,
    type_position: Position,
    position: Position,
    span: Span,
}

fn parameter(
    open: &TokenWithSpan,
    name: &TokenWithSpan,
    colon: &TokenWithSpan,
    ty: &TokenWithSpan,
    close: &TokenWithSpan,
) -> Option<Found> {
    if !matches!(colon.token, Token::Colon) || !matches!(close.token, Token::RBrace) {
        return None;
    }
    let name_word = unquoted_word(&name.token)?;
    let type_word = unquoted_word(&ty.token)?;
    Some(Found {
        name: name_word.value.clone(),
        type_name: type_word.value.clone(),
        type_position: token_position(ty),
        position: token_position(open),
        span: open.span.union(&close.span),
    })
}

fn reject_raw_placeholder(token: &TokenWithSpan) -> Result<(), InvalidQuery> {
    if matches!(token.token, Token::Placeholder(_)) {
        return Err(InvalidQuery::new(
            "write parameters as {name:Type}",
            Some(token_position(token)),
        ));
    }
    Ok(())
}

fn unquoted_word(token: &Token) -> Option<&Word> {
    match token {
        Token::Word(word) if word.quote_style.is_none() => Some(word),
        _ => None,
    }
}

fn convert(placeholder: &Placeholder, value: &Value) -> Result<ScalarValue, QueryError> {
    match placeholder.kind {
        ParamKind::String => match value {
            Value::String(text) => Ok(ScalarValue::Utf8(text.clone())),
            _ => Err(must_be(placeholder)),
        },
        ParamKind::Int => match value {
            Value::Number(number) => Ok(ScalarValue::I64(integer(number, &placeholder.name)?)),
            _ => Err(must_be(placeholder)),
        },
        ParamKind::Float => match value {
            Value::Number(number) => match number.as_f64() {
                Some(f) if f.is_finite() => Ok(ScalarValue::F64(f)),
                _ => Err(QueryError::invalid_request(format!(
                    "parameter {} is out of range",
                    placeholder.name
                ))),
            },
            _ => Err(must_be(placeholder)),
        },
        ParamKind::Bool => match value {
            Value::Bool(b) => Ok(ScalarValue::Bool(*b)),
            _ => Err(must_be(placeholder)),
        },
        ParamKind::Timestamp => timestamp(placeholder, value),
    }
}

fn integer(number: &Number, name: &str) -> Result<i64, QueryError> {
    if let Some(value) = number.as_i64() {
        return Ok(value);
    }
    if number.as_u64().is_some() {
        return Err(QueryError::invalid_request(format!(
            "parameter {name} is out of range"
        )));
    }
    Err(QueryError::invalid_request(format!(
        "parameter {name} must be an integer"
    )))
}

fn timestamp(placeholder: &Placeholder, value: &Value) -> Result<ScalarValue, QueryError> {
    let nanos = match value {
        Value::String(text) => parse_strict_rfc3339(text, placeholder)?,
        Value::Number(number) => {
            let Some(millis) = number.as_i64() else {
                if number.as_u64().is_some() {
                    return Err(QueryError::invalid_request(format!(
                        "parameter {} is out of range",
                        placeholder.name
                    )));
                }
                return Err(must_be(placeholder));
            };
            millis.checked_mul(1_000_000).ok_or_else(|| {
                QueryError::invalid_request(format!(
                    "parameter {} is out of range",
                    placeholder.name
                ))
            })?
        }
        _ => return Err(must_be(placeholder)),
    };
    Ok(ScalarValue::TimestampNs {
        value: nanos,
        utc: true,
    })
}

/// Parses strict RFC 3339 (requires `T`/`t`, `<= 9` fractional digits, no leap seconds,
/// and `i64` nanosecond range via `chrono::DateTime::parse_from_rfc3339`).
fn parse_strict_rfc3339(text: &str, placeholder: &Placeholder) -> Result<i64, QueryError> {
    let bytes = text.as_bytes();
    if !matches!(bytes.get(10), Some(b'T' | b't')) || fractional_digits_exceed_nine(bytes) {
        return Err(must_be(placeholder));
    }
    let parsed = DateTime::parse_from_rfc3339(text).map_err(|_| must_be(placeholder))?;
    if parsed.nanosecond() >= 1_000_000_000 {
        return Err(must_be(placeholder));
    }
    parsed
        .with_timezone(&Utc)
        .timestamp_nanos_opt()
        .ok_or_else(|| {
            QueryError::invalid_request(format!("parameter {} is out of range", placeholder.name))
        })
}

fn fractional_digits_exceed_nine(bytes: &[u8]) -> bool {
    if bytes.get(19) != Some(&b'.') {
        return false;
    }
    let mut digits = 0;
    for byte in bytes.iter().skip(20) {
        if !byte.is_ascii_digit() {
            break;
        }
        digits += 1;
        if digits > 9 {
            return true;
        }
    }
    false
}

fn must_be(placeholder: &Placeholder) -> QueryError {
    QueryError::invalid_request(format!(
        "parameter {} must be {}",
        placeholder.name,
        placeholder.kind.expectation()
    ))
}

fn token_position(token: &TokenWithSpan) -> Position {
    position_of(token.span.start.line, token.span.start.column)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::text::{SQL_LIMITS, TextKind, tokenize};

    fn scan(sql: &str) -> Result<(Vec<TokenWithSpan>, Vec<Placeholder>), InvalidQuery> {
        let tokens = tokenize(sql, &SQL_LIMITS, TextKind::Sql)?;
        bind_placeholders(tokens)
    }

    fn pos(line: u32, column: u32) -> Position {
        Position { line, column }
    }

    #[test]
    fn placeholder_scanning_rewriting_and_rejections() {
        let (tokens, phs) = scan(
            "SELECT * FROM spans WHERE name = {n:string} AND span_count >= {c:INT} OR name = {n:String}",
        )
        .unwrap();
        assert_eq!(phs.len(), 2);
        assert_eq!(phs[0].name, "n");
        assert_eq!(phs[0].kind, ParamKind::String);
        assert_eq!(phs[1].name, "c");
        assert_eq!(phs[1].kind, ParamKind::Int);

        let placeholder_tokens: Vec<String> = tokens
            .iter()
            .filter_map(|t| match &t.token {
                Token::Placeholder(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(placeholder_tokens, vec!["$1", "$2", "$1"]);

        // String literal is NOT rewritten.
        let (lit_tokens, lit_phs) = scan("SELECT '{n:String}' AS s").unwrap();
        assert!(lit_phs.is_empty());
        assert!(
            !lit_tokens
                .iter()
                .any(|t| matches!(t.token, Token::Placeholder(_)))
        );

        // Raw $1 and ? rejected with exact message & position.
        for sql in ["SELECT $1", "SELECT ?"] {
            let err = scan(sql).unwrap_err();
            assert_eq!(err.message, "write parameters as {name:Type}");
            assert_eq!(err.position, Some(pos(1, 8)));
        }

        // Unknown type points at the type token.
        let err_ty = scan("SELECT {x:Date}").unwrap_err();
        assert_eq!(
            err_ty.message,
            "unknown parameter type `Date`; use String, Int, Float, Bool or Timestamp"
        );
        assert_eq!(err_ty.position, Some(pos(1, 11)));

        // Conflicting redeclaration points at the second type token.
        let err_dup = scan("SELECT {n:String}, {n:Int}").unwrap_err();
        assert_eq!(err_dup.message, "parameter `n` is declared with two types");
        assert_eq!(err_dup.position, Some(pos(1, 23)));
    }

    #[test]
    fn json_param_parsing_validation_and_privacy_debug() {
        let (_, phs) =
            scan("SELECT {s:String}, {i:Int}, {f:Float}, {b:Bool}, {t:Timestamp}").unwrap();
        let params: Map<String, Value> = serde_json::from_value(json!({
            "s": "secret-prompt-text",
            "i": 42,
            "f": 3.5,
            "b": true,
            "t": "2026-01-01T10:05:00.123456789Z"
        }))
        .unwrap();
        let bound = parse_param_values(&phs, &params).unwrap();
        assert_eq!(bound.len(), 5);
        assert_eq!(
            bound[0].value,
            ScalarValue::Utf8("secret-prompt-text".to_owned())
        );
        assert_eq!(bound[1].value, ScalarValue::I64(42));
        assert_eq!(bound[2].value, ScalarValue::F64(3.5));
        assert_eq!(bound[3].value, ScalarValue::Bool(true));
        assert!(matches!(
            bound[4].value,
            ScalarValue::TimestampNs { utc: true, .. }
        ));

        // Privacy-safe Debug never logs parameter values.
        let dbg = format!("{bound:?}");
        assert!(!dbg.contains("secret-prompt-text"), "leaked param in {dbg}");
        assert!(dbg.contains("string"));

        // Epoch-ms Timestamp.
        let (_, t_phs) = scan("SELECT {t:Timestamp}").unwrap();
        let ms_map: Map<String, Value> =
            serde_json::from_value(json!({"t": 1_700_000_000_123i64})).unwrap();
        let ms_vals = parse_param_values(&t_phs, &ms_map).unwrap();
        assert_eq!(
            ms_vals[0].value,
            ScalarValue::TimestampNs {
                value: 1_700_000_000_123_000_000,
                utc: true
            }
        );

        // Space-separated non-RFC3339 string rejected for Timestamp (strict RFC 3339).
        let space_map: Map<String, Value> =
            serde_json::from_value(json!({"t": "2026-01-01 10:05:00Z"})).unwrap();
        let err = parse_param_values(&t_phs, &space_map).unwrap_err();
        assert!(matches!(
            err,
            QueryError::InvalidRequest(ref m) if m == "parameter t must be an RFC 3339 string or an integer"
        ));

        // Missing / unknown / out-of-range / wrong-type rejections.
        let (_, n_phs) = scan("SELECT {n:String}").unwrap();
        let err_missing = parse_param_values(&n_phs, &Map::new()).unwrap_err();
        assert!(matches!(
            err_missing,
            QueryError::InvalidRequest(ref m) if m == "missing value for parameter n"
        ));

        let extra: Map<String, Value> = serde_json::from_value(json!({"extra": 1})).unwrap();
        let err_extra = parse_param_values(&[], &extra).unwrap_err();
        assert!(matches!(
            err_extra,
            QueryError::InvalidRequest(ref m) if m == "unknown parameter extra"
        ));

        let (_, i_phs) = scan("SELECT {c:Int}").unwrap();
        for bad in [json!(1.5), json!("1")] {
            let m: Map<String, Value> = serde_json::from_value(json!({"c": bad})).unwrap();
            let e = parse_param_values(&i_phs, &m).unwrap_err();
            assert!(matches!(
                e,
                QueryError::InvalidRequest(ref msg) if msg == "parameter c must be an integer"
            ));
        }

        let u64_over: Map<String, Value> = serde_json::from_value(json!({"c": u64::MAX})).unwrap();
        let err_u64 = parse_param_values(&i_phs, &u64_over).unwrap_err();
        assert!(matches!(
            err_u64,
            QueryError::InvalidRequest(ref msg) if msg == "parameter c is out of range"
        ));

        let ts_over: Map<String, Value> = serde_json::from_value(json!({"t": i64::MAX})).unwrap();
        let err_ts = parse_param_values(&t_phs, &ts_over).unwrap_err();
        assert!(matches!(
            err_ts,
            QueryError::InvalidRequest(ref msg) if msg == "parameter t is out of range"
        ));
    }

    #[test]
    fn usage_site_param_coercion() {
        let p_str = SqlParamValue {
            name: "s".to_owned(),
            value: ScalarValue::Utf8("42".to_owned()),
        };
        assert_eq!(
            coerce_param_value(&p_str, &DataType::I64).unwrap(),
            ScalarValue::I64(42)
        );

        let p_bad = SqlParamValue {
            name: "s".to_owned(),
            value: ScalarValue::Utf8("abc".to_owned()),
        };
        let err = coerce_param_value(&p_bad, &DataType::I64).unwrap_err();
        assert!(matches!(
            err,
            QueryError::InvalidRequest(ref m) if m == "parameter s does not fit where it is used (integer)"
        ));
    }
}
