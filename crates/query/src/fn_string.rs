//! Vectorized string, binary, `LIKE`/`ILIKE`, regex, and encoding functions.

use std::sync::Arc;

use cetra_column::{
    Bitmap, Column, ColumnError, DataType, FlatColumn, FlatData, Reservation, ScalarValue, Utf8Data,
};
use regex::{Regex, RegexBuilder};

use crate::error::QueryError;
use crate::eval::{
    check_cancel, checked_varlen_bytes, is_active, pool_size, read_bytes, read_i64, read_str,
    reconcile_output, rollback_to, try_reserve,
};
use crate::ir::{EvalContext, ScalarFn};

const CANCEL_CHECK_MASK: usize = 1023;
const REGEX_SIZE_LIMIT: usize = 256 * 1024;
const REGEX_DFA_SIZE_LIMIT: usize = 128 * 1024;
pub(crate) const REGEX_ESTIMATED_BYTES: usize = 384 * 1024;
const MAX_CONCAT_BYTES: usize = i32::MAX as usize;

fn bitmap_mem_bytes(len: usize) -> usize {
    len.div_ceil(64) * 8
}

fn check_col_len(col: &Column, expected: usize) -> Result<(), QueryError> {
    if col.len() != expected {
        return Err(ColumnError::InvalidBatch {
            reason: "argument column length mismatch",
        }
        .into());
    }
    Ok(())
}

fn check_active_len(active: Option<&Bitmap>, expected: usize) -> Result<(), QueryError> {
    if active.is_some_and(|bm| bm.len() != expected) {
        return Err(ColumnError::InvalidBatch {
            reason: "active bitmap length mismatch",
        }
        .into());
    }
    Ok(())
}

fn expect_utf8_or_null(col: &Column) -> Result<(), QueryError> {
    if !matches!(col.data_type(), DataType::Utf8 | DataType::Null) {
        return Err(QueryError::failed("expected string argument"));
    }
    Ok(())
}

fn expect_i64_or_null(col: &Column) -> Result<(), QueryError> {
    if !matches!(col.data_type(), DataType::I64 | DataType::Null) {
        return Err(QueryError::failed("expected integer argument"));
    }
    Ok(())
}

fn null_const(data_type: DataType, len: usize) -> Column {
    Column::Const {
        value: ScalarValue::Null(data_type),
        len,
    }
}

fn build_bool_column(values: Bitmap, nulls: Option<Bitmap>) -> Result<Column, QueryError> {
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Bool(values),
        nulls,
    )?))
}

fn finish_with_pool(
    res: Result<Column, QueryError>,
    mut pool: Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    match res {
        Ok(col) => {
            reconcile_output(&mut pool, before, col.memory_bytes())?;
            Ok(col)
        }
        Err(err) => {
            rollback_to(&mut pool, before);
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// LIKE / ILIKE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum LikeToken {
    Literal(String),
    AnySeq,
    AnyChar,
}

pub(crate) enum LikeMatcher {
    Any,
    Empty,
    Exact {
        lit: String,
        ci_regex: Option<Regex>,
    },
    Prefix {
        prefix: String,
        ci_regex: Option<Regex>,
    },
    Suffix {
        suffix: String,
        ci_regex: Option<Regex>,
    },
    Contains {
        mid: String,
        ci_regex: Option<Regex>,
    },
    Regex(Regex),
}

impl LikeMatcher {
    pub(crate) fn estimated_bytes(&self) -> usize {
        let extra = match self {
            Self::Any | Self::Empty => 0,
            Self::Exact { lit, ci_regex } => {
                lit.len()
                    + if ci_regex.is_some() {
                        REGEX_ESTIMATED_BYTES
                    } else {
                        0
                    }
            }
            Self::Prefix { prefix, ci_regex } => {
                prefix.len()
                    + if ci_regex.is_some() {
                        REGEX_ESTIMATED_BYTES
                    } else {
                        0
                    }
            }
            Self::Suffix { suffix, ci_regex } => {
                suffix.len()
                    + if ci_regex.is_some() {
                        REGEX_ESTIMATED_BYTES
                    } else {
                        0
                    }
            }
            Self::Contains { mid, ci_regex } => {
                mid.len()
                    + if ci_regex.is_some() {
                        REGEX_ESTIMATED_BYTES
                    } else {
                        0
                    }
            }
            Self::Regex(_) => REGEX_ESTIMATED_BYTES,
        };
        size_of::<Self>() + extra
    }

    pub(crate) fn compile(
        pattern: &str,
        escape: Option<char>,
        case_insensitive: bool,
    ) -> Result<Self, QueryError> {
        let tokens = tokenize_like_pattern(pattern, escape);
        match tokens.as_slice() {
            [] => Ok(Self::Empty),
            [LikeToken::AnySeq] => Ok(Self::Any),
            [LikeToken::Literal(lit)] => {
                let ci_regex = if case_insensitive {
                    Some(compile_like_regex(&tokens, true)?)
                } else {
                    None
                };
                Ok(Self::Exact {
                    lit: lit.clone(),
                    ci_regex,
                })
            }
            [LikeToken::Literal(prefix), LikeToken::AnySeq] => {
                let ci_regex = if case_insensitive {
                    Some(compile_like_regex(&tokens, true)?)
                } else {
                    None
                };
                Ok(Self::Prefix {
                    prefix: prefix.clone(),
                    ci_regex,
                })
            }
            [LikeToken::AnySeq, LikeToken::Literal(suffix)] => {
                let ci_regex = if case_insensitive {
                    Some(compile_like_regex(&tokens, true)?)
                } else {
                    None
                };
                Ok(Self::Suffix {
                    suffix: suffix.clone(),
                    ci_regex,
                })
            }
            [
                LikeToken::AnySeq,
                LikeToken::Literal(mid),
                LikeToken::AnySeq,
            ] => {
                let ci_regex = if case_insensitive {
                    Some(compile_like_regex(&tokens, true)?)
                } else {
                    None
                };
                Ok(Self::Contains {
                    mid: mid.clone(),
                    ci_regex,
                })
            }
            _ => Ok(Self::Regex(compile_like_regex(&tokens, case_insensitive)?)),
        }
    }

    fn is_match(&self, s: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Empty => s.is_empty(),
            Self::Exact { lit, ci_regex } => match ci_regex {
                None => s == lit,
                Some(re) => {
                    if s.is_ascii() && lit.is_ascii() {
                        s.eq_ignore_ascii_case(lit)
                    } else {
                        re.is_match(s)
                    }
                }
            },
            Self::Prefix { prefix, ci_regex } => match ci_regex {
                None => s.starts_with(prefix.as_str()),
                Some(re) => {
                    if s.is_ascii() && prefix.is_ascii() {
                        s.len() >= prefix.len()
                            && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
                    } else {
                        re.is_match(s)
                    }
                }
            },
            Self::Suffix { suffix, ci_regex } => match ci_regex {
                None => s.ends_with(suffix.as_str()),
                Some(re) => {
                    if s.is_ascii() && suffix.is_ascii() {
                        s.len() >= suffix.len()
                            && s.as_bytes()[s.len() - suffix.len()..]
                                .eq_ignore_ascii_case(suffix.as_bytes())
                    } else {
                        re.is_match(s)
                    }
                }
            },
            Self::Contains { mid, ci_regex } => match ci_regex {
                None => s.contains(mid.as_str()),
                Some(re) => {
                    if s.is_ascii() && mid.is_ascii() && mid.len() <= 32 {
                        if mid.is_empty() {
                            true
                        } else {
                            s.as_bytes()
                                .windows(mid.len())
                                .any(|w| w.eq_ignore_ascii_case(mid.as_bytes()))
                        }
                    } else {
                        re.is_match(s)
                    }
                }
            },
            Self::Regex(re) => re.is_match(s),
        }
    }
}

fn tokenize_like_pattern(pattern: &str, escape: Option<char>) -> Vec<LikeToken> {
    let mut tokens: Vec<LikeToken> = Vec::new();
    let mut chars = pattern.chars();
    let mut lit_buf = String::new();

    let flush_lit = |buf: &mut String, out: &mut Vec<LikeToken>| {
        if !buf.is_empty() {
            out.push(LikeToken::Literal(std::mem::take(buf)));
        }
    };

    while let Some(ch) = chars.next() {
        if escape == Some(ch) {
            if let Some(escaped) = chars.next() {
                lit_buf.push(escaped);
            } else {
                // Trailing lone escape matches itself literally (DataFusion / arrow-string parity).
                lit_buf.push(ch);
            }
        } else if ch == '%' {
            flush_lit(&mut lit_buf, &mut tokens);
            if !matches!(tokens.last(), Some(LikeToken::AnySeq)) {
                tokens.push(LikeToken::AnySeq);
            }
        } else if ch == '_' {
            flush_lit(&mut lit_buf, &mut tokens);
            tokens.push(LikeToken::AnyChar);
        } else {
            lit_buf.push(ch);
        }
    }
    flush_lit(&mut lit_buf, &mut tokens);
    tokens
}

fn compile_like_regex(tokens: &[LikeToken], case_insensitive: bool) -> Result<Regex, QueryError> {
    let mut re_str = String::from(r"\A");
    for tok in tokens {
        match tok {
            LikeToken::Literal(s) => re_str.push_str(&regex::escape(s)),
            LikeToken::AnySeq => re_str.push_str(".*"),
            LikeToken::AnyChar => re_str.push('.'),
        }
    }
    re_str.push_str(r"\z");

    RegexBuilder::new(&re_str)
        .case_insensitive(case_insensitive)
        .dot_matches_new_line(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
        .build()
        .map_err(|_| QueryError::failed("invalid or oversized LIKE pattern"))
}

/// Evaluates SQL `[NOT] LIKE / ILIKE ... [ESCAPE ...]` over `input` and `pattern`.
///
/// Follows 3-valued SQL NULL logic, skips inactive rows under `active`, handles all 4 `Column`
/// encodings, and reconciles `pool` to `before + col.memory_bytes()` on `Ok` (rolling back on `Err`).
#[expect(
    clippy::too_many_arguments,
    reason = "public contract signature specified by QE2.2"
)]
pub fn eval_like(
    input: &Column,
    pattern: &Column,
    escape: Option<char>,
    case_insensitive: bool,
    negated: bool,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let res = eval_like_inner(
        input,
        pattern,
        escape,
        case_insensitive,
        negated,
        active,
        ctx,
        &mut pool,
        before,
    );
    finish_with_pool(res, pool, before)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper mirrors eval_like parameters plus reservation baseline"
)]
fn eval_like_inner(
    input: &Column,
    pattern: &Column,
    escape: Option<char>,
    case_insensitive: bool,
    negated: bool,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    check_cancel(ctx, pool, before)?;
    let len = input.len();
    check_col_len(pattern, len)?;
    check_active_len(active, len)?;
    expect_utf8_or_null(input)?;
    expect_utf8_or_null(pattern)?;

    if matches!(input.data_type(), DataType::Null)
        || matches!(pattern.data_type(), DataType::Null)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Bool, len));
    }

    if matches!(
        input,
        Column::Const {
            value: ScalarValue::Null(_),
            ..
        }
    ) || matches!(
        pattern,
        Column::Const {
            value: ScalarValue::Null(_),
            ..
        }
    ) {
        return Ok(null_const(DataType::Bool, len));
    }

    let const_pat = match pattern {
        Column::Const {
            value: ScalarValue::Utf8(p),
            ..
        } => Some(p.as_str()),
        _ => None,
    };
    if len == 0 {
        return Ok(null_const(DataType::Bool, len));
    }
    let mut cached_const: Option<Arc<LikeMatcher>> = None;
    let mut last_dyn: Option<(&str, Arc<LikeMatcher>)> = None;

    let bm_bytes = bitmap_mem_bytes(len);
    try_reserve(pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(len);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(s) = read_str(input, row) else {
            continue;
        };
        let matched = if let Some(pat) = const_pat {
            let matcher = match &cached_const {
                Some(m) => m,
                None => {
                    let compiled =
                        ctx.cache
                            .get_or_insert_like(pat, escape, case_insensitive, || {
                                LikeMatcher::compile(pat, escape, case_insensitive)
                            })?;
                    cached_const.insert(compiled)
                }
            };
            matcher.is_match(s)
        } else {
            let Some(pat) = read_str(pattern, row) else {
                continue;
            };
            let hit = matches!(last_dyn.as_ref(), Some((prev, _)) if *prev == pat);
            if !hit {
                let compiled =
                    ctx.cache
                        .get_or_insert_like(pat, escape, case_insensitive, || {
                            LikeMatcher::compile(pat, escape, case_insensitive)
                        })?;
                last_dyn = Some((pat, compiled));
            }
            match last_dyn.as_ref() {
                Some((_, matcher)) => matcher.is_match(s),
                None => false,
            }
        };

        let _ = nulls.set(row, true);
        if matched ^ negated {
            let _ = vals.set(row, true);
        }
    }

    build_bool_column(vals, Some(nulls))
}

// ---------------------------------------------------------------------------
// String concatenation (||)
// ---------------------------------------------------------------------------

/// Evaluates SQL `left || right` string concatenation.
///
/// Propagates `NULL` if either operand is `NULL`, enforces the 2 GiB UTF-8 result cap
/// (`i32::MAX as usize`), skips inactive rows under `active`, and reconciles `pool`.
pub fn eval_string_concat(
    left: &Column,
    right: &Column,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let res = eval_string_concat_inner(left, right, active, ctx, &mut pool, before);
    finish_with_pool(res, pool, before)
}

fn eval_string_concat_inner(
    left: &Column,
    right: &Column,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    check_cancel(ctx, pool, before)?;
    let len = left.len();
    check_col_len(right, len)?;
    check_active_len(active, len)?;
    expect_utf8_or_null(left)?;
    expect_utf8_or_null(right)?;

    if matches!(left.data_type(), DataType::Null)
        || matches!(right.data_type(), DataType::Null)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    let mut total_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let (Some(l), Some(r)) = (read_str(left, row), read_str(right, row)) {
            total_bytes = add_concat_bytes(total_bytes, l.len())?;
            total_bytes = add_concat_bytes(total_bytes, r.len())?;
        }
    }

    let est = (len + 1)
        .checked_mul(4)
        .and_then(|o| o.checked_add(total_bytes))
        .and_then(|o| o.checked_add(bitmap_mem_bytes(len)))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let (Some(l), Some(r)) = (read_str(left, row), read_str(right, row))
        {
            bytes.extend_from_slice(l.as_bytes());
            bytes.extend_from_slice(r.as_bytes());
            let _ = nulls.set(row, true);
        }
        offsets.push(bytes.len() as u32);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

fn add_concat_bytes(acc: usize, add: usize) -> Result<usize, QueryError> {
    let next = acc
        .checked_add(add)
        .filter(|&v| v <= MAX_CONCAT_BYTES)
        .ok_or_else(|| QueryError::failed("string concatenation result is larger than 2 GiB"))?;
    Ok(next)
}

// ---------------------------------------------------------------------------
// ScalarFn dispatch
// ---------------------------------------------------------------------------

/// Evaluates any string or binary [`ScalarFn`] over `args` of row count `len`.
///
/// Enforces active-row masking under `active`, supports all 4 `Column` encodings, checks
/// cancellation every 1024 rows, and maintains the exact `Reservation` postcondition.
pub fn eval_string_fn(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let refs: Vec<&Column> = args.iter().collect();
    eval_string_fn_refs(func, &refs, len, active, ctx, pool)
}

pub(crate) fn eval_string_fn_refs(
    func: ScalarFn,
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let res = eval_string_fn_inner(func, args, len, active, ctx, &mut pool, before);
    finish_with_pool(res, pool, before)
}

fn eval_string_fn_inner(
    func: ScalarFn,
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    check_cancel(ctx, pool, before)?;
    check_active_len(active, len)?;
    for &col in args {
        check_col_len(col, len)?;
    }

    match func {
        ScalarFn::Lower => eval_case_change(args, len, active, ctx, pool, before, false),
        ScalarFn::Upper => eval_case_change(args, len, active, ctx, pool, before, true),
        ScalarFn::Length => eval_length(args, len, active, ctx, pool, before),
        ScalarFn::Substr => eval_substr(args, len, active, ctx, pool, before),
        ScalarFn::Trim => eval_trim(args, len, active, ctx, pool, before, TrimKind::Both),
        ScalarFn::Ltrim => eval_trim(args, len, active, ctx, pool, before, TrimKind::Start),
        ScalarFn::Rtrim => eval_trim(args, len, active, ctx, pool, before, TrimKind::End),
        ScalarFn::Left => eval_left_right(args, len, active, ctx, pool, before, true),
        ScalarFn::Right => eval_left_right(args, len, active, ctx, pool, before, false),
        ScalarFn::SplitPart => eval_split_part(args, len, active, ctx, pool, before),
        ScalarFn::Concat => eval_concat(args, len, active, ctx, pool, before),
        ScalarFn::ConcatWs => eval_concat_ws(args, len, active, ctx, pool, before),
        ScalarFn::StartsWith => eval_str_predicate(args, len, active, ctx, pool, before, |s, p| {
            s.starts_with(p)
        }),
        ScalarFn::EndsWith => {
            eval_str_predicate(args, len, active, ctx, pool, before, |s, p| s.ends_with(p))
        }
        ScalarFn::Contains => {
            eval_str_predicate(args, len, active, ctx, pool, before, |s, p| s.contains(p))
        }
        ScalarFn::Strpos => eval_strpos(args, len, active, ctx, pool, before),
        ScalarFn::RegexpLike => eval_regexp_like(args, len, active, ctx, pool, before),
        ScalarFn::RegexpExtract => eval_regexp_extract(args, len, active, ctx, pool, before),
        ScalarFn::Encode => eval_encode(args, len, active, ctx, pool, before),
        _ => Err(QueryError::failed("not a string or binary scalar function")),
    }
}

// ---------------------------------------------------------------------------
// lower / upper
// ---------------------------------------------------------------------------

fn eval_case_change(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
    to_upper: bool,
) -> Result<Column, QueryError> {
    let &[input] = args else {
        return Err(QueryError::failed("lower/upper takes 1 argument"));
    };
    expect_utf8_or_null(input)?;
    if matches!(input.data_type(), DataType::Null) || active.is_some_and(Bitmap::all_zeros) {
        return Ok(null_const(DataType::Utf8, len));
    }

    let mut total_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let Some(s) = read_str(input, row) {
            let out_len = if s.is_ascii() {
                s.len()
            } else if to_upper {
                s.chars()
                    .flat_map(char::to_uppercase)
                    .map(char::len_utf8)
                    .sum()
            } else {
                s.chars()
                    .flat_map(char::to_lowercase)
                    .map(char::len_utf8)
                    .sum()
            };
            total_bytes = total_bytes
                .checked_add(out_len)
                .ok_or(ColumnError::Overflow {
                    reason: "utf8 byte length overflow",
                })?;
        }
    }
    let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
        reason: "utf8 byte length exceeds u32::MAX",
    })?;
    let est = (len + 1)
        .checked_mul(4)
        .and_then(|o| o.checked_add(total_bytes))
        .and_then(|o| o.checked_add(bitmap_mem_bytes(len)))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(s) = read_str(input, row)
        {
            if s.is_ascii() {
                let start = bytes.len();
                bytes.extend_from_slice(s.as_bytes());
                if to_upper {
                    bytes[start..].make_ascii_uppercase();
                } else {
                    bytes[start..].make_ascii_lowercase();
                }
            } else if to_upper {
                bytes.extend_from_slice(s.to_uppercase().as_bytes());
            } else {
                bytes.extend_from_slice(s.to_lowercase().as_bytes());
            }
            let _ = nulls.set(row, true);
        }
        let end = u32::try_from(bytes.len()).map_err(|_| ColumnError::Overflow {
            reason: "utf8 byte length exceeds u32::MAX",
        })?;
        offsets.push(end);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// length
// ---------------------------------------------------------------------------

fn eval_length(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let &[input] = args else {
        return Err(QueryError::failed("length takes 1 argument"));
    };
    expect_utf8_or_null(input)?;
    if matches!(input.data_type(), DataType::Null) || active.is_some_and(Bitmap::all_zeros) {
        return Ok(null_const(DataType::I64, len));
    }

    let est = len
        .checked_mul(8)
        .and_then(|o| o.checked_add(bitmap_mem_bytes(len)))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut vals = vec![0i64; len];
    let mut nulls = Bitmap::new_zeroed(len);
    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(s) = read_str(input, row)
        {
            *slot = s.chars().count() as i64;
            let _ = nulls.set(row, true);
        }
    }
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::I64(vals),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// Shared two-pass builder for slice-returning Utf8 functions
// ---------------------------------------------------------------------------

fn eval_utf8_slice_fn<'a>(
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
    mut f: impl FnMut(usize) -> Result<Option<&'a str>, QueryError>,
) -> Result<Column, QueryError> {
    let mut total_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let Some(out) = f(row)? {
            total_bytes = total_bytes
                .checked_add(out.len())
                .ok_or(ColumnError::Overflow {
                    reason: "utf8 byte length overflow",
                })?;
        }
    }
    let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
        reason: "utf8 byte length exceeds u32::MAX",
    })?;
    let est = (len + 1)
        .checked_mul(4)
        .and_then(|o| o.checked_add(total_bytes))
        .and_then(|o| o.checked_add(bitmap_mem_bytes(len)))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(out) = f(row)?
        {
            bytes.extend_from_slice(out.as_bytes());
            let _ = nulls.set(row, true);
        }
        offsets.push(bytes.len() as u32);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// substr
// ---------------------------------------------------------------------------

fn substr_slice(s: &str, start: i64, count: Option<i64>) -> Result<&str, QueryError> {
    let start_1 = i128::from(start);
    let end_1 = match count {
        Some(c) => {
            if c < 0 {
                return Err(QueryError::failed("negative substring length not allowed"));
            }
            start_1 + i128::from(c)
        }
        None => i128::MAX,
    };

    let clamped_start = start_1.max(1);
    let clamped_end = end_1.max(clamped_start);
    let take_chars = usize::try_from(clamped_end - clamped_start).unwrap_or(usize::MAX);
    if take_chars == 0 {
        return Ok("");
    }
    let Ok(skip_chars) = usize::try_from(clamped_start - 1) else {
        return Ok("");
    };

    let mut char_iter = s.char_indices();
    let Some((byte_start, _)) = char_iter.nth(skip_chars) else {
        return Ok("");
    };
    if take_chars == usize::MAX {
        return Ok(&s[byte_start..]);
    }
    match char_iter.nth(take_chars - 1) {
        Some((byte_end, _)) => Ok(&s[byte_start..byte_end]),
        None => Ok(&s[byte_start..]),
    }
}

fn eval_substr(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let (s_col, start_col, count_col) = match *args {
        [s, start] => (s, start, None),
        [s, start, count] => (s, start, Some(count)),
        _ => return Err(QueryError::failed("substr takes 2 or 3 arguments")),
    };
    expect_utf8_or_null(s_col)?;
    expect_i64_or_null(start_col)?;
    if let Some(c) = count_col {
        expect_i64_or_null(c)?;
    }

    if matches!(s_col.data_type(), DataType::Null)
        || matches!(start_col.data_type(), DataType::Null)
        || count_col.is_some_and(|c| matches!(c.data_type(), DataType::Null))
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    eval_utf8_slice_fn(len, active, ctx, pool, before, |row| {
        let Some(s) = read_str(s_col, row) else {
            return Ok(None);
        };
        let Some(start) = read_i64(start_col, row) else {
            return Ok(None);
        };
        let count = match count_col {
            Some(c_col) => match read_i64(c_col, row) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
            None => None,
        };
        substr_slice(s, start, count).map(Some)
    })
}

// ---------------------------------------------------------------------------
// trim / ltrim / rtrim
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum TrimKind {
    Both,
    Start,
    End,
}

fn eval_trim(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
    kind: TrimKind,
) -> Result<Column, QueryError> {
    let (s_col, chars_col) = match *args {
        [s] => (s, None),
        [s, chars] => (s, Some(chars)),
        _ => return Err(QueryError::failed("trim takes 1 or 2 arguments")),
    };
    expect_utf8_or_null(s_col)?;
    if let Some(c) = chars_col {
        expect_utf8_or_null(c)?;
    }

    if matches!(s_col.data_type(), DataType::Null)
        || chars_col.is_some_and(|c| matches!(c.data_type(), DataType::Null))
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    eval_utf8_slice_fn(len, active, ctx, pool, before, |row| {
        let Some(s) = read_str(s_col, row) else {
            return Ok(None);
        };
        let out = match chars_col {
            None => match kind {
                TrimKind::Both => s.trim_matches(' '),
                TrimKind::Start => s.trim_start_matches(' '),
                TrimKind::End => s.trim_end_matches(' '),
            },
            Some(c_col) => {
                let Some(chars) = read_str(c_col, row) else {
                    return Ok(None);
                };
                match kind {
                    TrimKind::Both => s.trim_matches(|ch| chars.contains(ch)),
                    TrimKind::Start => s.trim_start_matches(|ch| chars.contains(ch)),
                    TrimKind::End => s.trim_end_matches(|ch| chars.contains(ch)),
                }
            }
        };
        Ok(Some(out))
    })
}

// ---------------------------------------------------------------------------
// left / right
// ---------------------------------------------------------------------------

fn slice_left_or_right(s: &str, n: i64, is_left: bool) -> &str {
    if n == 0 {
        return "";
    }
    let abs_n = usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX);
    if is_left {
        if n > 0 {
            match s.char_indices().nth(abs_n) {
                Some((end, _)) => &s[..end],
                None => s,
            }
        } else {
            // Negative n drops |n| chars from the right end.
            match s.char_indices().rev().nth(abs_n.saturating_sub(1)) {
                Some((end, _)) => &s[..end],
                None => "",
            }
        }
    } else if n > 0 {
        match s.char_indices().rev().nth(abs_n.saturating_sub(1)) {
            Some((start, _)) => &s[start..],
            None => s,
        }
    } else {
        // Negative n drops |n| chars from the left start.
        match s.char_indices().nth(abs_n) {
            Some((start, _)) => &s[start..],
            None => "",
        }
    }
}

fn eval_left_right(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
    is_left: bool,
) -> Result<Column, QueryError> {
    let &[s_col, n_col] = args else {
        return Err(QueryError::failed("left/right takes 2 arguments"));
    };
    expect_utf8_or_null(s_col)?;
    expect_i64_or_null(n_col)?;

    if matches!(s_col.data_type(), DataType::Null)
        || matches!(n_col.data_type(), DataType::Null)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    eval_utf8_slice_fn(len, active, ctx, pool, before, |row| {
        let Some(s) = read_str(s_col, row) else {
            return Ok(None);
        };
        let Some(n) = read_i64(n_col, row) else {
            return Ok(None);
        };
        Ok(Some(slice_left_or_right(s, n, is_left)))
    })
}

// ---------------------------------------------------------------------------
// split_part
// ---------------------------------------------------------------------------

fn split_part_slice<'a>(s: &'a str, delim: &str, n: i64) -> Result<&'a str, QueryError> {
    if n == 0 {
        return Err(QueryError::failed("field position must not be zero"));
    }
    if delim.is_empty() {
        return Ok(if n == 1 || n == -1 { s } else { "" });
    }
    if n > 0 {
        let Ok(idx) = usize::try_from(n - 1) else {
            return Ok("");
        };
        Ok(s.split(delim).nth(idx).unwrap_or(""))
    } else {
        let Ok(k) = usize::try_from(n.unsigned_abs()) else {
            return Ok("");
        };
        Ok(s.rsplit(delim).nth(k - 1).unwrap_or(""))
    }
}

fn eval_split_part(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let &[s_col, delim_col, n_col] = args else {
        return Err(QueryError::failed("split_part takes 3 arguments"));
    };
    expect_utf8_or_null(s_col)?;
    expect_utf8_or_null(delim_col)?;
    expect_i64_or_null(n_col)?;

    if matches!(s_col.data_type(), DataType::Null)
        || matches!(delim_col.data_type(), DataType::Null)
        || matches!(n_col.data_type(), DataType::Null)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    eval_utf8_slice_fn(len, active, ctx, pool, before, |row| {
        let Some(s) = read_str(s_col, row) else {
            return Ok(None);
        };
        let Some(delim) = read_str(delim_col, row) else {
            return Ok(None);
        };
        let Some(n) = read_i64(n_col, row) else {
            return Ok(None);
        };
        split_part_slice(s, delim, n).map(Some)
    })
}

// ---------------------------------------------------------------------------
// concat / concat_ws
// ---------------------------------------------------------------------------

fn eval_concat(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if args.is_empty() {
        return Err(QueryError::failed("concat requires at least 1 argument"));
    }
    for &col in args {
        expect_utf8_or_null(col)?;
    }

    if active.is_some_and(Bitmap::all_zeros) {
        return Ok(null_const(DataType::Utf8, len));
    }

    let mut total_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        for &col in args {
            if let Some(s) = read_str(col, row) {
                total_bytes = add_concat_bytes(total_bytes, s.len())?;
            }
        }
    }

    let need_nulls = active.is_some();
    let nulls_bytes = if need_nulls { bitmap_mem_bytes(len) } else { 0 };
    let est = (len + 1)
        .checked_mul(4)
        .and_then(|o| o.checked_add(total_bytes))
        .and_then(|o| o.checked_add(nulls_bytes))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = need_nulls.then(|| Bitmap::new_zeroed(len));

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row) {
            if let Some(n) = nulls.as_mut() {
                let _ = n.set(row, true);
            }
            for &col in args {
                if let Some(s) = read_str(col, row) {
                    bytes.extend_from_slice(s.as_bytes());
                }
            }
        }
        offsets.push(bytes.len() as u32);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        nulls,
    )?))
}

fn eval_concat_ws(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if args.len() < 2 {
        return Err(QueryError::failed(
            "concat_ws requires at least 2 arguments",
        ));
    }
    for &col in args {
        expect_utf8_or_null(col)?;
    }

    let sep_col = args[0];
    let val_cols = &args[1..];
    if matches!(sep_col.data_type(), DataType::Null) || active.is_some_and(Bitmap::all_zeros) {
        return Ok(null_const(DataType::Utf8, len));
    }

    let mut total_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(sep) = read_str(sep_col, row) else {
            continue;
        };
        let mut first = true;
        for &col in val_cols {
            if let Some(s) = read_str(col, row) {
                if !first {
                    total_bytes = add_concat_bytes(total_bytes, sep.len())?;
                }
                first = false;
                total_bytes = add_concat_bytes(total_bytes, s.len())?;
            }
        }
    }

    try_reserve(pool, checked_varlen_bytes(len, total_bytes, true)?)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(sep) = read_str(sep_col, row)
        {
            let _ = nulls.set(row, true);
            let mut first = true;
            for &col in val_cols {
                if let Some(s) = read_str(col, row) {
                    if !first {
                        bytes.extend_from_slice(sep.as_bytes());
                    }
                    first = false;
                    bytes.extend_from_slice(s.as_bytes());
                }
            }
        }
        offsets.push(bytes.len() as u32);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// starts_with / ends_with / contains / strpos
// ---------------------------------------------------------------------------

fn eval_str_predicate(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
    pred: fn(&str, &str) -> bool,
) -> Result<Column, QueryError> {
    let &[s_col, pat_col] = args else {
        return Err(QueryError::failed("string predicate takes 2 arguments"));
    };
    expect_utf8_or_null(s_col)?;
    expect_utf8_or_null(pat_col)?;

    if matches!(s_col.data_type(), DataType::Null)
        || matches!(pat_col.data_type(), DataType::Null)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Bool, len));
    }

    let bm_bytes = bitmap_mem_bytes(len);
    try_reserve(pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(len);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let (Some(s), Some(p)) = (read_str(s_col, row), read_str(pat_col, row)) {
            let _ = nulls.set(row, true);
            if pred(s, p) {
                let _ = vals.set(row, true);
            }
        }
    }
    build_bool_column(vals, Some(nulls))
}

fn eval_strpos(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let &[s_col, sub_col] = args else {
        return Err(QueryError::failed("strpos takes 2 arguments"));
    };
    expect_utf8_or_null(s_col)?;
    expect_utf8_or_null(sub_col)?;

    if matches!(s_col.data_type(), DataType::Null)
        || matches!(sub_col.data_type(), DataType::Null)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::I64, len));
    }

    let est = len
        .checked_mul(8)
        .and_then(|o| o.checked_add(bitmap_mem_bytes(len)))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut vals = vec![0i64; len];
    let mut nulls = Bitmap::new_zeroed(len);

    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let (Some(s), Some(sub)) = (read_str(s_col, row), read_str(sub_col, row)) {
            *slot = if sub.is_empty() {
                1
            } else {
                match s.find(sub) {
                    Some(byte_idx) => (s[..byte_idx].chars().count() + 1) as i64,
                    None => 0,
                }
            };
            let _ = nulls.set(row, true);
        }
    }

    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::I64(vals),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// regexp_like / regexp_extract
// ---------------------------------------------------------------------------

fn compile_regex(pat: &str, flags: &str) -> Result<Regex, QueryError> {
    let mut case_insensitive = false;
    let mut multi_line = false;
    let mut dot_matches_new_line = false;
    for ch in flags.chars() {
        match ch {
            'i' => case_insensitive = true,
            'm' => multi_line = true,
            's' => dot_matches_new_line = true,
            _ => return Err(QueryError::failed("unsupported regex flag")),
        }
    }
    RegexBuilder::new(pat)
        .case_insensitive(case_insensitive)
        .multi_line(multi_line)
        .dot_matches_new_line(dot_matches_new_line)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
        .build()
        .map_err(|_| QueryError::failed("invalid or oversized regex pattern"))
}

enum RegexpCache<'a> {
    Const(Option<Arc<Regex>>),
    Dynamic {
        last: Option<(&'a str, &'a str, Arc<Regex>)>,
    },
}

impl<'a> RegexpCache<'a> {
    fn new(pat_col: &'a Column, flags_col: Option<&'a Column>) -> Self {
        let is_const_pat = matches!(
            pat_col,
            Column::Const {
                value: ScalarValue::Utf8(_),
                ..
            }
        );
        let is_const_flags = matches!(
            flags_col,
            None | Some(Column::Const {
                value: ScalarValue::Utf8(_),
                ..
            })
        );
        if is_const_pat && is_const_flags {
            Self::Const(None)
        } else {
            Self::Dynamic { last: None }
        }
    }

    fn get_or_compile(
        &mut self,
        pat: &'a str,
        flags: &'a str,
        ctx: &EvalContext,
    ) -> Result<Arc<Regex>, QueryError> {
        match self {
            Self::Const(slot) => {
                if let Some(re) = slot.as_ref() {
                    return Ok(Arc::clone(re));
                }
                let re = ctx
                    .cache
                    .get_or_insert_regex(pat, flags, || compile_regex(pat, flags))?;
                *slot = Some(Arc::clone(&re));
                Ok(re)
            }
            Self::Dynamic { last } => {
                if let Some((p, f, re)) = last.as_ref()
                    && *p == pat
                    && *f == flags
                {
                    return Ok(Arc::clone(re));
                }
                let re = ctx
                    .cache
                    .get_or_insert_regex(pat, flags, || compile_regex(pat, flags))?;
                *last = Some((pat, flags, Arc::clone(&re)));
                Ok(re)
            }
        }
    }
}

fn check_regexp_args<'a>(
    args: &[&'a Column],
) -> Result<(&'a Column, &'a Column, Option<&'a Column>), QueryError> {
    let (s_col, pat_col, flags_col) = match *args {
        [s, pat] => (s, pat, None),
        [s, pat, flags] => (s, pat, Some(flags)),
        _ => return Err(QueryError::failed("regexp function takes 2 or 3 arguments")),
    };
    expect_utf8_or_null(s_col)?;
    expect_utf8_or_null(pat_col)?;
    if let Some(f) = flags_col {
        expect_utf8_or_null(f)?;
    }
    Ok((s_col, pat_col, flags_col))
}

fn has_null_regexp_arg_type(s_col: &Column, pat_col: &Column, flags_col: Option<&Column>) -> bool {
    matches!(s_col.data_type(), DataType::Null)
        || matches!(pat_col.data_type(), DataType::Null)
        || flags_col.is_some_and(|c| matches!(c.data_type(), DataType::Null))
        || matches!(
            s_col,
            Column::Const {
                value: ScalarValue::Null(_),
                ..
            }
        )
        || matches!(
            pat_col,
            Column::Const {
                value: ScalarValue::Null(_),
                ..
            }
        )
        || flags_col.is_some_and(|c| {
            matches!(
                c,
                Column::Const {
                    value: ScalarValue::Null(_),
                    ..
                }
            )
        })
}

fn eval_regexp_like(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let (s_col, pat_col, flags_col) = check_regexp_args(args)?;
    if len == 0
        || has_null_regexp_arg_type(s_col, pat_col, flags_col)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Bool, len));
    }

    let mut cache = RegexpCache::new(pat_col, flags_col);
    let bm_bytes = bitmap_mem_bytes(len);
    try_reserve(pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(len);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(s) = read_str(s_col, row) else {
            continue;
        };
        let Some(pat) = read_str(pat_col, row) else {
            continue;
        };
        let flags = match flags_col {
            Some(fc) => match read_str(fc, row) {
                Some(f) => f,
                None => continue,
            },
            None => "",
        };
        let re = cache.get_or_compile(pat, flags, ctx)?;
        let _ = nulls.set(row, true);
        if re.is_match(s) {
            let _ = vals.set(row, true);
        }
    }

    build_bool_column(vals, Some(nulls))
}

fn regexp_extract_match<'a>(re: &Regex, s: &'a str) -> Option<&'a str> {
    if re.captures_len() > 1 {
        re.captures(s)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str())
    } else {
        re.find(s).map(|m| m.as_str())
    }
}

fn eval_regexp_extract(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let (s_col, pat_col, flags_col) = check_regexp_args(args)?;
    if len == 0
        || has_null_regexp_arg_type(s_col, pat_col, flags_col)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    let mut cache = RegexpCache::new(pat_col, flags_col);
    try_reserve(pool, checked_varlen_bytes(len, 0, true)?)?;
    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(s) = read_str(s_col, row)
            && let Some(pat) = read_str(pat_col, row)
            && let Some(flags) = match flags_col {
                Some(fc) => read_str(fc, row),
                None => Some(""),
            }
        {
            let re = cache.get_or_compile(pat, flags, ctx)?;
            if let Some(m) = regexp_extract_match(&re, s) {
                let next_len = bytes
                    .len()
                    .checked_add(m.len())
                    .ok_or(ColumnError::Overflow {
                        reason: "utf8 byte length overflow",
                    })?;
                let _ = u32::try_from(next_len).map_err(|_| ColumnError::Overflow {
                    reason: "utf8 byte length exceeds u32::MAX",
                })?;
                try_reserve(pool, m.len())?;
                bytes.extend_from_slice(m.as_bytes());
                let _ = nulls.set(row, true);
            }
        }
        offsets.push(bytes.len() as u32);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// encode
// ---------------------------------------------------------------------------

fn read_encode_bytes(col: &Column, row: usize) -> Result<Option<&[u8]>, QueryError> {
    match col.data_type() {
        DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
            Ok(read_bytes(col, row))
        }
        DataType::Utf8 => Ok(read_str(col, row).map(str::as_bytes)),
        DataType::Null => Ok(None),
        _ => Err(QueryError::failed("encode requires binary or string input")),
    }
}

fn eval_encode(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let &[val_col, enc_col] = args else {
        return Err(QueryError::failed("encode takes 2 arguments"));
    };
    if !matches!(
        val_col.data_type(),
        DataType::Binary
            | DataType::FixedBytes8
            | DataType::FixedBytes16
            | DataType::Utf8
            | DataType::Null
    ) {
        return Err(QueryError::failed("encode requires binary or string input"));
    }
    expect_utf8_or_null(enc_col)?;

    if len == 0
        || matches!(enc_col.data_type(), DataType::Null)
        || active.is_some_and(Bitmap::all_zeros)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    let mut total_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(enc) = read_str(enc_col, row) else {
            continue;
        };
        if !enc.eq_ignore_ascii_case("hex") {
            return Err(QueryError::failed(
                "unsupported encoding; only 'hex' is supported",
            ));
        }
        let Some(src) = read_encode_bytes(val_col, row)? else {
            continue;
        };
        let hex_len = src.len().checked_mul(2).ok_or(ColumnError::Overflow {
            reason: "hex encoded byte length overflow",
        })?;
        total_bytes = total_bytes
            .checked_add(hex_len)
            .ok_or(ColumnError::Overflow {
                reason: "utf8 byte length overflow",
            })?;
    }
    if matches!(val_col.data_type(), DataType::Null) {
        return Ok(null_const(DataType::Utf8, len));
    }
    let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
        reason: "utf8 byte length exceeds u32::MAX",
    })?;
    let est = (len + 1)
        .checked_mul(4)
        .and_then(|o| o.checked_add(total_bytes))
        .and_then(|o| o.checked_add(bitmap_mem_bytes(len)))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(src) = read_encode_bytes(val_col, row)?
            && read_str(enc_col, row).is_some()
        {
            let start = bytes.len();
            bytes.resize(start + src.len() * 2, 0);
            if hex::encode_to_slice(src, &mut bytes[start..]).is_err() {
                return Err(QueryError::failed("hex encoding failed"));
            }
            let _ = nulls.set(row, true);
        }
        offsets.push(bytes.len() as u32);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use cetra_column::{BinaryData, CancelToken, DictColumn, GlobalPool, RleColumn};

    use super::*;

    fn ctx() -> EvalContext {
        EvalContext::new(0, CancelToken::new())
    }

    fn utf8_col(vals: &[Option<&str>]) -> Column {
        let mut offsets = Vec::with_capacity(vals.len() + 1);
        offsets.push(0u32);
        let mut bytes = Vec::new();
        let mut nulls = Bitmap::new_zeroed(vals.len());
        for (i, v) in vals.iter().enumerate() {
            if let Some(s) = v {
                bytes.extend_from_slice(s.as_bytes());
                let _ = nulls.set(i, true);
            }
            offsets.push(bytes.len() as u32);
        }
        let u = Utf8Data::try_new(offsets, bytes).unwrap();
        Column::Flat(FlatColumn::try_new(FlatData::Utf8(u), Some(nulls)).unwrap())
    }

    fn i64_col(vals: &[Option<i64>]) -> Column {
        let mut out = vec![0i64; vals.len()];
        let mut nulls = Bitmap::new_zeroed(vals.len());
        for (i, v) in vals.iter().enumerate() {
            if let Some(n) = v {
                out[i] = *n;
                let _ = nulls.set(i, true);
            }
        }
        Column::Flat(FlatColumn::try_new(FlatData::I64(out), Some(nulls)).unwrap())
    }

    fn const_str(s: &str, len: usize) -> Column {
        Column::try_new_const(ScalarValue::Utf8(s.to_owned()), len).unwrap()
    }

    fn const_i64(n: i64, len: usize) -> Column {
        Column::try_new_const(ScalarValue::I64(n), len).unwrap()
    }

    fn bool_vec(col: &Column) -> Vec<Option<bool>> {
        (0..col.len())
            .map(|r| {
                if col.is_null(r) {
                    None
                } else {
                    crate::eval::read_bool(col, r)
                }
            })
            .collect()
    }

    fn str_vec(col: &Column) -> Vec<Option<String>> {
        (0..col.len())
            .map(|r| read_str(col, r).map(str::to_owned))
            .collect()
    }

    fn i64_vec(col: &Column) -> Vec<Option<i64>> {
        (0..col.len()).map(|r| read_i64(col, r)).collect()
    }

    #[test]
    fn like_and_ilike_with_escape_and_unicode() {
        // Basic %, _, \n matching, trailing lone escape, literal \% and \_, None escape
        let input = utf8_col(&[
            Some("hello\nworld"),
            Some("a%b_c"),
            Some("aXbYc"),
            Some("foo\\"),
            Some("Café Noir"),
            Some("東京🚀タワー"),
            None,
            Some("should_be_masked"),
        ]);
        let len = input.len();
        let active = Bitmap::from_bools(&[true, true, true, true, true, true, true, false]);

        // % and _ match newlines
        let pat_nl = const_str("hello_world", len);
        let res = eval_like(
            &input,
            &pat_nl,
            Some('\\'),
            false,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&res),
            vec![
                Some(true),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                None,
                None,
            ]
        );

        let pat_pct_nl = const_str("hello%world", len);
        let res = eval_like(
            &input,
            &pat_pct_nl,
            Some('\\'),
            false,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&res)[0], Some(true));

        // Escaped \% and \_ match literally, not aXbYc
        let pat_esc = const_str(r"a\%b\_c", len);
        let res = eval_like(
            &input,
            &pat_esc,
            Some('\\'),
            false,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&res)[1], Some(true));
        assert_eq!(bool_vec(&res)[2], Some(false));

        // escape = None disables backslash escaping
        let pat_no_esc = const_str(r"a\%b\_c", len);
        let res = eval_like(
            &input,
            &pat_no_esc,
            None,
            false,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&res)[1], Some(false));

        // Trailing lone escape matches literal escape char
        let pat_trailing = const_str("foo\\", len);
        let res = eval_like(
            &input,
            &pat_trailing,
            Some('\\'),
            false,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&res)[3], Some(true));

        // Unicode ILIKE: accents, CJK, emoji, NOT ILIKE, NULL propagation
        let pat_cafe = const_str("%café%", len);
        let res = eval_like(
            &input,
            &pat_cafe,
            Some('\\'),
            true,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&res)[4], Some(true));
        assert_eq!(bool_vec(&res)[6], None);
        assert_eq!(bool_vec(&res)[7], None);

        let res_not = eval_like(
            &input,
            &pat_cafe,
            Some('\\'),
            true,
            true,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&res_not)[4], Some(false));
        assert_eq!(bool_vec(&res_not)[0], Some(true));
        assert_eq!(bool_vec(&res_not)[6], None);

        // CJK + emoji with _ and %
        let pat_cjk = const_str("東京_タ%", len);
        let res_cjk = eval_like(
            &input,
            &pat_cjk,
            Some('\\'),
            true,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&res_cjk)[5], Some(true));

        // Non-const pattern across Dict and Rle encodings
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["%café%", "hello%"]).unwrap()),
            None,
        )
        .unwrap();
        let dict_pat = Column::Dict(
            DictColumn::try_new(dict_vals, vec![1, 0, 0, 0, 0, 0, 0, 0], None).unwrap(),
        );
        let rle_in = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["hello\nworld", "CAFÉ"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![1, 8],
            )
            .unwrap(),
        );
        let res_enc = eval_like(
            &rle_in,
            &dict_pat,
            Some('\\'),
            true,
            false,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&res_enc),
            vec![
                Some(true),
                Some(true),
                Some(true),
                Some(true),
                Some(true),
                Some(true),
                Some(true),
                None,
            ]
        );
    }

    #[test]
    fn string_functions_edge_cases() {
        // Substr: Postgres/DataFusion 1-based clamping & negative length error
        let s = const_str("abcde", 5);
        let starts = i64_col(&[Some(0), Some(-1), Some(-5), Some(2), Some(4)]);
        let counts = i64_col(&[Some(3), Some(4), Some(2), Some(0), Some(10)]);
        let sub_res = eval_string_fn(
            ScalarFn::Substr,
            &[s, starts, counts],
            5,
            None,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            str_vec(&sub_res),
            vec![
                Some("ab".into()),
                Some("ab".into()),
                Some("".into()),
                Some("".into()),
                Some("de".into()),
            ]
        );

        let sub_2arg = eval_string_fn(
            ScalarFn::Substr,
            &[const_str("abcde", 2), i64_col(&[Some(0), Some(3)])],
            2,
            None,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            str_vec(&sub_2arg),
            vec![Some("abcde".into()), Some("cde".into())]
        );

        let neg_err = eval_string_fn(
            ScalarFn::Substr,
            &[const_str("abcde", 1), const_i64(1, 1), const_i64(-1, 1)],
            1,
            None,
            &ctx(),
            None,
        )
        .unwrap_err();
        assert!(matches!(neg_err, QueryError::Failed(_)));

        // Trim / Ltrim / Rtrim: 1-arg trims ASCII ' ' ONLY!
        let ws = const_str("  \ta \n ", 1);
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Trim,
                    std::slice::from_ref(&ws),
                    1,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![Some("\ta \n".into())]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Ltrim,
                    std::slice::from_ref(&ws),
                    1,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![Some("\ta \n ".into())]
        );
        assert_eq!(
            str_vec(&eval_string_fn(ScalarFn::Rtrim, &[ws], 1, None, &ctx(), None).unwrap()),
            vec![Some("  \ta \n".into())]
        );
        let trim_2arg = eval_string_fn(
            ScalarFn::Trim,
            &[const_str("xyxhelloyxy", 1), const_str("xy", 1)],
            1,
            None,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(str_vec(&trim_2arg), vec![Some("hello".into())]);

        // Left / Right (including negative n and Unicode)
        let lr_s = const_str("abcde", 4);
        let lr_n = i64_col(&[Some(2), Some(-2), Some(10), Some(-10)]);
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Left,
                    &[lr_s.clone(), lr_n.clone()],
                    4,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![
                Some("ab".into()),
                Some("abc".into()),
                Some("abcde".into()),
                Some("".into()),
            ]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(ScalarFn::Right, &[lr_s, lr_n], 4, None, &ctx(), None).unwrap()
            ),
            vec![
                Some("de".into()),
                Some("cde".into()),
                Some("abcde".into()),
                Some("".into()),
            ]
        );

        // SplitPart: positive, negative, 0 -> Failed, empty delim, overlapping delim
        let sp_s = utf8_col(&[
            Some("a.b.c"),
            Some("a.b.c"),
            Some("a.b.c"),
            Some("abc"),
            Some("aaa"),
        ]);
        let sp_d = utf8_col(&[Some("."), Some("."), Some("."), Some(""), Some("aa")]);
        let sp_n = i64_col(&[Some(2), Some(-1), Some(5), Some(-1), Some(-1)]);
        let sp_res = eval_string_fn(
            ScalarFn::SplitPart,
            &[sp_s, sp_d, sp_n],
            5,
            None,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            str_vec(&sp_res),
            vec![
                Some("b".into()),
                Some("c".into()),
                Some("".into()),
                Some("abc".into()),
                Some("".into()),
            ]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::SplitPart,
                    &[const_str("abc", 1), const_str("", 1), const_i64(2, 1)],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some("".into())]
        );
        let sp_zero = eval_string_fn(
            ScalarFn::SplitPart,
            &[const_str("a.b", 1), const_str(".", 1), const_i64(0, 1)],
            1,
            None,
            &ctx(),
            None,
        )
        .unwrap_err();
        assert!(matches!(sp_zero, QueryError::Failed(_)));

        // Strpos, StartsWith, EndsWith, Contains, Lower, Upper, Length
        assert_eq!(
            i64_vec(
                &eval_string_fn(
                    ScalarFn::Strpos,
                    &[
                        const_str("héllo", 3),
                        utf8_col(&[Some(""), Some("llo"), Some("z")])
                    ],
                    3,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some(1), Some(3), Some(0)]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Lower,
                    &[const_str("Café Σ", 1)],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some("café σ".into())]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Upper,
                    &[const_str("café", 1)],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some("CAFÉ".into())]
        );
        assert_eq!(
            i64_vec(
                &eval_string_fn(
                    ScalarFn::Length,
                    &[const_str("🚀ab", 1)],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some(3)]
        );
        assert_eq!(
            bool_vec(
                &eval_string_fn(
                    ScalarFn::StartsWith,
                    &[const_str("foobar", 1), const_str("foo", 1)],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some(true)]
        );
        assert_eq!(
            bool_vec(
                &eval_string_fn(
                    ScalarFn::EndsWith,
                    &[const_str("foobar", 1), const_str("bar", 1)],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some(true)]
        );
        assert_eq!(
            bool_vec(
                &eval_string_fn(
                    ScalarFn::Contains,
                    &[const_str("foobar", 1), const_str("oob", 1)],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some(true)]
        );

        // Concat vs ConcatWs vs ||
        let a1 = utf8_col(&[Some("a"), None, Some("x")]);
        let a2 = utf8_col(&[Some("b"), Some("c"), None]);
        let sep = utf8_col(&[Some("-"), Some("-"), None]);
        assert_eq!(
            str_vec(&eval_string_concat(&a1, &a2, None, &ctx(), None).unwrap()),
            vec![Some("ab".into()), None, None]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Concat,
                    &[a1.clone(), a2.clone()],
                    3,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![Some("ab".into()), Some("c".into()), Some("x".into())]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(ScalarFn::ConcatWs, &[sep, a1, a2], 3, None, &ctx(), None).unwrap()
            ),
            vec![Some("a-b".into()), Some("c".into()), None]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::ConcatWs,
                    &[
                        const_str(",", 1),
                        Column::try_new_const(ScalarValue::Null(DataType::Utf8), 1).unwrap(),
                    ],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some("".into())]
        );

        // Encode hex on FixedBytes16, FixedBytes8, Binary, Utf8, and error on unknown encoding
        let fb16 = Column::try_new_const(
            ScalarValue::FixedBytes16([
                0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0xaa, 0xff,
            ]),
            1,
        )
        .unwrap();
        let fb8 = Column::try_new_const(
            ScalarValue::FixedBytes8([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]),
            1,
        )
        .unwrap();
        let bin = Column::Flat(
            FlatColumn::try_new(
                FlatData::Binary(BinaryData::try_from_slices([b"\xca\xfe".as_slice()]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let hex_enc = const_str("HEX", 1);
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Encode,
                    &[fb16, hex_enc.clone()],
                    1,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![Some("deadbeef00010203040506070809aaff".into())]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Encode,
                    &[fb8, hex_enc.clone()],
                    1,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![Some("0123456789abcdef".into())]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Encode,
                    &[bin, hex_enc.clone()],
                    1,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![Some("cafe".into())]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::Encode,
                    &[const_str("abc", 1), hex_enc],
                    1,
                    None,
                    &ctx(),
                    None
                )
                .unwrap()
            ),
            vec![Some("616263".into())]
        );
        let bad_enc = eval_string_fn(
            ScalarFn::Encode,
            &[const_str("abc", 1), const_str("base64", 1)],
            1,
            None,
            &ctx(),
            None,
        )
        .unwrap_err();
        assert!(matches!(bad_enc, QueryError::Failed(_)));

        // RegexpExtract with capture group, without capture group, non-participating group 1, flags
        let re_in = utf8_col(&[Some("id=42"), Some("no_digits"), Some("b")]);
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::RegexpExtract,
                    &[re_in.clone(), const_str(r"id=(\d+)", 3)],
                    3,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some("42".into()), None, None]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::RegexpExtract,
                    &[re_in.clone(), const_str(r"\d+", 3)],
                    3,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some("42".into()), None, None]
        );
        assert_eq!(
            str_vec(
                &eval_string_fn(
                    ScalarFn::RegexpExtract,
                    &[re_in, const_str("(a)|(b)", 3)],
                    3,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![None, None, None]
        );
        assert_eq!(
            bool_vec(
                &eval_string_fn(
                    ScalarFn::RegexpLike,
                    &[
                        const_str("Hello\nWorld", 1),
                        const_str("hello.world", 1),
                        const_str("is", 1),
                    ],
                    1,
                    None,
                    &ctx(),
                    None,
                )
                .unwrap()
            ),
            vec![Some(true)]
        );
        let bad_flag = eval_string_fn(
            ScalarFn::RegexpLike,
            &[const_str("abc", 1), const_str("abc", 1), const_str("g", 1)],
            1,
            None,
            &ctx(),
            None,
        )
        .unwrap_err();
        assert!(matches!(bad_flag, QueryError::Failed(_)));
    }

    #[test]
    fn regex_size_limit_and_concat_memory_cap() {
        // 1. Oversized regex fails with QueryError::Failed
        let huge_pat = const_str("(((a{100}){100}){100})", 1);
        let err = eval_string_fn(
            ScalarFn::RegexpLike,
            &[const_str("a", 1), huge_pat],
            1,
            None,
            &ctx(),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Failed(_)));

        // 2. Tight reservation on concat / concat_ws -> TooLarge with pool.size() == before
        let scope = GlobalPool::standalone(256);
        let mut res = scope.register("fn_string", false);
        res.try_grow(32).unwrap();
        let before = res.size();

        let big = const_str(&"x".repeat(200), 2);
        let err_concat = eval_string_concat(&big, &big, None, &ctx(), Some(&mut res)).unwrap_err();
        assert!(matches!(err_concat, QueryError::TooLarge));
        assert_eq!(res.size(), before);

        // Exact reservation on concat_ws: mostly-null args with large unused separator succeed,
        // while actual non-null separator amplification returns TooLarge and rolls back.
        let sep_big = const_str(&"-".repeat(50), 2);
        let n1 = Column::try_new_const(ScalarValue::Null(DataType::Utf8), 2).unwrap();
        let mostly_null_ws = eval_string_fn(
            ScalarFn::ConcatWs,
            &[
                sep_big.clone(),
                const_str("a", 2),
                n1.clone(),
                n1.clone(),
                n1.clone(),
                n1,
            ],
            2,
            None,
            &ctx(),
            Some(&mut res),
        )
        .unwrap();
        assert_eq!(
            str_vec(&mostly_null_ws),
            vec![Some("a".into()), Some("a".into())]
        );
        assert_eq!(res.size(), before + mostly_null_ws.memory_bytes());
        res.shrink(mostly_null_ws.memory_bytes());
        assert_eq!(res.size(), before);

        let err_ws = eval_string_fn(
            ScalarFn::ConcatWs,
            &[
                sep_big,
                const_str("a", 2),
                const_str("b", 2),
                const_str("c", 2),
                const_str("d", 2),
            ],
            2,
            None,
            &ctx(),
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(err_ws, QueryError::TooLarge));
        assert_eq!(res.size(), before);

        // 3. Ok reconciles to before + col.memory_bytes()
        let ok_scope = GlobalPool::standalone(4096);
        let mut ok_res = ok_scope.register("fn_string_ok", false);
        ok_res.try_grow(24).unwrap();
        let ok_before = ok_res.size();
        let out = eval_string_fn(
            ScalarFn::ConcatWs,
            &[
                const_str(",", 2),
                const_str("hello", 2),
                Column::try_new_const(ScalarValue::Null(DataType::Utf8), 2).unwrap(),
                const_str("world", 2),
            ],
            2,
            None,
            &ctx(),
            Some(&mut ok_res),
        )
        .unwrap();
        assert_eq!(ok_res.size(), ok_before + out.memory_bytes());
        assert_eq!(
            str_vec(&out),
            vec![Some("hello,world".into()), Some("hello,world".into())]
        );

        // 4. Masked-out row with invalid regex or split_part(..., 0) does not fault
        let active = Bitmap::from_bools(&[true, false]);
        let pats = utf8_col(&[Some("[0-9]+"), Some("[invalid")]);
        let masked_re = eval_string_fn(
            ScalarFn::RegexpLike,
            &[utf8_col(&[Some("123"), Some("bad")]), pats],
            2,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&masked_re), vec![Some(true), None]);

        let masked_sp = eval_string_fn(
            ScalarFn::SplitPart,
            &[
                const_str("a.b", 2),
                const_str(".", 2),
                i64_col(&[Some(2), Some(0)]),
            ],
            2,
            Some(&active),
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(str_vec(&masked_sp), vec![Some("b".into()), None]);

        // 5. 2 GiB cap error on string concat when theoretical output > i32::MAX
        let huge = const_str(&"a".repeat(1024 * 1024), 2049);
        let cap_err = eval_string_concat(&huge, &huge, None, &ctx(), None).unwrap_err();
        assert!(matches!(cap_err, QueryError::Failed(_)));

        // 6. CancelToken abort
        let cancelled = EvalContext::new(0, CancelToken::new());
        cancelled.cancel.cancel();
        let cancel_err = eval_string_fn(
            ScalarFn::Upper,
            &[const_str("abc", 3)],
            3,
            None,
            &cancelled,
            None,
        )
        .unwrap_err();
        assert!(matches!(cancel_err, QueryError::Cancelled));

        // 7. Dict & Rle encodings on eval_string_fn
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["  hi ", "BYE"]).unwrap()),
            None,
        )
        .unwrap();
        let dict_col = Column::Dict(DictColumn::try_new(dict_vals, vec![0, 1, 0], None).unwrap());
        let rle_col = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["-", ":"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![2, 3],
            )
            .unwrap(),
        );
        let trimmed = eval_string_fn(ScalarFn::Trim, &[dict_col], 3, None, &ctx(), None).unwrap();
        let joined = eval_string_fn(
            ScalarFn::ConcatWs,
            &[rle_col, trimmed, const_str("end", 3)],
            3,
            None,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            str_vec(&joined),
            vec![
                Some("hi-end".into()),
                Some("BYE-end".into()),
                Some("hi:end".into()),
            ]
        );
    }

    #[test]
    fn ilike_long_needle_encode_null_validation_and_regexp_extract_oom() {
        // Blocker 5: long ASCII ILIKE '%mid%' (>32 chars) delegates to linear-time ci_regex.
        let long_needle = format!("%{}Z%", "a".repeat(64));
        let hay = utf8_col(&[
            Some(&format!("{}z", "A".repeat(200))),
            Some(&"A".repeat(200)),
        ]);
        let pat = const_str(&long_needle, 2);
        let res = eval_like(&hay, &pat, None, true, false, None, &ctx(), None).unwrap();
        assert_eq!(bool_vec(&res), vec![Some(true), Some(false)]);

        // Major 4: split_part("aaa", "aa", -2) == "a" alongside -1 == ""
        let sp = eval_string_fn(
            ScalarFn::SplitPart,
            &[const_str("aaa", 1), const_str("aa", 1), const_i64(-2, 1)],
            1,
            None,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(str_vec(&sp), vec![Some("a".into())]);

        // Minor 4: encode validates encoding upfront on active rows even when input is NULL.
        let null_col = Column::Const {
            value: ScalarValue::Null(DataType::Null),
            len: 2,
        };
        let bad_enc = utf8_col(&[Some("base64"), Some("hex")]);
        assert!(matches!(
            eval_string_fn(
                ScalarFn::Encode,
                &[null_col.clone(), bad_enc.clone()],
                2,
                None,
                &ctx(),
                None,
            )
            .unwrap_err(),
            QueryError::Failed(_)
        ));
        // Inactive row with invalid encoding must not fault.
        let mask = Bitmap::from_bools(&[false, true]);
        let masked_enc = eval_string_fn(
            ScalarFn::Encode,
            &[null_col, bad_enc],
            2,
            Some(&mask),
            &ctx(),
            None,
        )
        .unwrap();
        assert!(masked_enc.is_null(0));
        assert!(masked_enc.is_null(1));

        // Minor 3: eval_regexp_extract single-pass streaming OOM rollback.
        let pool = GlobalPool::standalone(48);
        let mut res = pool.register("re_oom", false);
        let s_col = const_str("prefix-payload_1234567890_abcdef", 4);
        let re_pat = const_str("(payload_[0-9a-z_]+)", 4);
        let oom = eval_string_fn(
            ScalarFn::RegexpExtract,
            &[s_col, re_pat],
            4,
            None,
            &ctx(),
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(oom, QueryError::TooLarge));
        assert_eq!(res.size(), 0);

        // H2-M2: lazy constant regex/LIKE compilation for NULL-input SQL semantics.
        let null_const_utf8 = Column::Const {
            value: ScalarValue::Null(DataType::Utf8),
            len: 3,
        };
        let all_null_flat = utf8_col(&[None, None, None]);
        let invalid_re = const_str("[unclosed", 3);

        for input_col in [&null_const_utf8, &all_null_flat] {
            let out_like =
                eval_regexp_like(&[input_col, &invalid_re], 3, None, &ctx(), &mut None, 0).unwrap();
            assert!((0..3).all(|r| out_like.is_null(r)));

            let out_ext =
                eval_regexp_extract(&[input_col, &invalid_re], 3, None, &ctx(), &mut None, 0)
                    .unwrap();
            assert!((0..3).all(|r| out_ext.is_null(r)));

            let oversized_like_pat = const_str(&"_".repeat(100_000), 3);
            let like_res = eval_like(
                input_col,
                &oversized_like_pat,
                Some('\\'),
                false,
                false,
                None,
                &ctx(),
                None,
            )
            .unwrap();
            assert!((0..3).all(|r| like_res.is_null(r)));
        }
    }
}
