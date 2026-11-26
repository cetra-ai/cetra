//! Columnar batches → API types (D-G-14).
//!
//! Columns are read by name across all four `cetra_column::Column` encodings
//! (`Flat`, `Const`, `Dict`, `Rle`) and all three `Selection` variants (`All`,
//! `Mask`, `Indices`).

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use cetra_column::{
    Batch, Bitmap, Column, DataType, EventItem, FlatColumn, FlatData, LinkItem, ScalarValue,
    Selection,
};
use cetra_core::enums::{
    Convention, CostSource, PayloadFormat, ScoreDataType, ScoreSource, ScoreTargetKind, SpanKind,
    SpanStatus,
};
use cetra_core::ids::{ProjectId, SpanId, TraceId};
use cetra_core::schema::{scores, spans, traces};
use cetra_core::score::{Score, ScoreTarget, ScoreValue};
use cetra_core::span::{Span, SpanEvent, SpanLink};
use uuid::Uuid;

use super::dashboards::{
    TotalRow, WidgetPoint, WidgetQueryResponse, WidgetSeries, WidgetSeriesResponse,
    WidgetTotalResponse,
};
use super::payload::Payload;
use super::schema::{ColumnSchema, ColumnType, FunctionSchema, SchemaResponse, ViewSchema};
use super::sessions::SessionSummary;
use super::traces::{ScoreBrief, SpanSummary, TraceSummary, stored_payload};
use super::users::{EndUserSummary, UserScore};
use crate::http::error::ApiError;

/// `duration_ns` of the traces view. The spans table has the same name.
const TRACE_DURATION_NS: &str = "duration_ns";
/// `target_key` of `latest_scores` (`LATEST_SCORE_COLUMNS`).
const TARGET_KEY: &str = "target_key";
/// `trace_count` of the sessions and users views.
const TRACE_COUNT: &str = "trace_count";
/// `session_count` of the users view.
const SESSION_COUNT: &str = "session_count";
/// `bucket` of the user activity batches.
const ACTIVITY_BUCKET: &str = "bucket";
/// `avg` of the user score batches.
const SCORE_AVG: &str = "avg";
/// `count` of the user score batches.
const SCORE_COUNT: &str = "count";
/// Prefix of the trace metadata keys shown on a user (spec 09 `EndUserSummary.metadata`).
const USER_METADATA_PREFIX: &str = "user.";
/// `first_seen` of the users view.
const FIRST_SEEN: &str = "first_seen";
/// `last_seen` of the users view.
const LAST_SEEN: &str = "last_seen";

/// A stored row does not have the shape the API expects. The text names the column, never a value.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConvertError {
    #[error("column {0} is missing")]
    Missing(&'static str),
    #[error("column {0} has an unexpected type")]
    Type(&'static str),
    #[error("column {0} is null")]
    Null(&'static str),
    #[error("column {0} has an invalid value")]
    Value(&'static str),
}

impl From<ConvertError> for ApiError {
    fn from(error: ConvertError) -> Self {
        Self::Internal(Box::new(error))
    }
}

// ---- Active row iteration ------------------------------------------------

/// Iterates physical row indices of active rows in `batch`.
pub(crate) fn active_rows(batch: &Batch) -> ActiveRows<'_> {
    match batch.selection() {
        Selection::All => ActiveRows::All(0..batch.raw_len()),
        Selection::Mask(mask) => ActiveRows::Mask(MaskWordIter::new(mask)),
        Selection::Indices(indices) => ActiveRows::Indices(indices.iter()),
    }
}

fn first_active_row(batch: &Batch) -> Option<usize> {
    active_rows(batch).next()
}

/// Physical row iterator over active rows in a [`Batch`].
pub(crate) enum ActiveRows<'a> {
    All(std::ops::Range<usize>),
    Mask(MaskWordIter<'a>),
    Indices(std::slice::Iter<'a, u32>),
}

/// Iterates set bit positions of a [`Bitmap`] via its backing `u64` words.
pub(crate) struct MaskWordIter<'a> {
    words: &'a [u64],
    len: usize,
    word_idx: usize,
    bit_base: usize,
    current_word: u64,
}

impl<'a> MaskWordIter<'a> {
    fn new(bm: &'a Bitmap) -> Self {
        let words = bm.words();
        Self {
            words,
            len: bm.len(),
            word_idx: 0,
            bit_base: 0,
            current_word: words.first().copied().unwrap_or(0),
        }
    }
}

impl Iterator for MaskWordIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current_word == 0 {
            self.word_idx += 1;
            if self.word_idx >= self.words.len() {
                return None;
            }
            self.current_word = self.words[self.word_idx];
            self.bit_base = self.word_idx * 64;
        }
        let tz = self.current_word.trailing_zeros() as usize;
        let pos = self.bit_base + tz;
        if pos >= self.len {
            self.current_word = 0;
            return None;
        }
        self.current_word &= self.current_word - 1;
        Some(pos)
    }
}

impl Iterator for ActiveRows<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::All(range) => range.next(),
            Self::Mask(iter) => iter.next(),
            Self::Indices(iter) => iter.next().map(|&idx| idx as usize),
        }
    }
}

// ---- Column access -------------------------------------------------------

pub(crate) struct Col<'a> {
    column: &'a Column,
    name: &'static str,
}

fn require_col<'a>(
    batch: &'a Batch,
    name: &'static str,
    check: impl FnOnce(&DataType) -> bool,
) -> Result<Col<'a>, ConvertError> {
    let column = batch
        .column_by_name(name)
        .ok_or(ConvertError::Missing(name))?;
    let dt = column.data_type();
    if !matches!(dt, DataType::Null) && !check(&dt) {
        return Err(ConvertError::Type(name));
    }
    Ok(Col { column, name })
}

pub(crate) fn strings<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| matches!(dt, DataType::Utf8))
}

fn ints<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| matches!(dt, DataType::I64))
}

fn floats<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| {
        matches!(
            dt,
            DataType::F64 | DataType::Decimal128 { .. } | DataType::I64
        )
    })
}

fn bools<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| matches!(dt, DataType::Bool))
}

pub(crate) fn times<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| matches!(dt, DataType::TimestampNs { .. }))
}

fn lists<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(
        batch,
        name,
        |dt| matches!(dt, DataType::List(inner) if matches!(inner.as_ref(), DataType::Utf8 | DataType::Null)),
    )
}

fn maps<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| matches!(dt, DataType::MapUtf8))
}

pub(crate) fn events_col<'a>(
    batch: &'a Batch,
    name: &'static str,
) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| matches!(dt, DataType::Events))
}

fn links_col<'a>(batch: &'a Batch, name: &'static str) -> Result<Col<'a>, ConvertError> {
    require_col(batch, name, |dt| matches!(dt, DataType::Links))
}

fn resolve_flat(col: &Column, phys_row: usize) -> Option<(&FlatColumn, usize)> {
    match col {
        Column::Flat(fc) => {
            (phys_row < fc.len() && !fc.is_null(phys_row)).then_some((fc, phys_row))
        }
        Column::Const { .. } => None,
        Column::Dict(d) => {
            if phys_row >= d.len() || d.is_null(phys_row) {
                return None;
            }
            let code = *d.codes().get(phys_row)? as usize;
            (!d.values().is_null(code)).then_some((d.values(), code))
        }
        Column::Rle(r) => {
            if phys_row >= r.len() {
                return None;
            }
            let run_idx = r
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            (!r.values().is_null(run_idx)).then_some((r.values(), run_idx))
        }
    }
}

// ---- Row values ----------------------------------------------------------

fn read_str<'a>(col: &Col<'a>, phys_row: usize) -> Result<Option<&'a str>, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::Utf8(s) => Ok(Some(s.as_str())),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::Utf8(utf8) => utf8
            .get(flat_row)
            .map(Some)
            .ok_or(ConvertError::Value(col.name)),
        FlatData::Null(_) => Ok(None),
        _ => Err(ConvertError::Type(col.name)),
    }
}

pub(crate) fn opt_string(col: &Col<'_>, phys_row: usize) -> Result<Option<String>, ConvertError> {
    read_str(col, phys_row).map(|opt| opt.map(str::to_owned))
}

fn req_string(col: &Col<'_>, phys_row: usize) -> Result<String, ConvertError> {
    opt_string(col, phys_row)?.ok_or(ConvertError::Null(col.name))
}

fn opt_i64(col: &Col<'_>, phys_row: usize) -> Result<Option<i64>, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::I64(v) => Ok(Some(*v)),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::I64(values) => Ok(Some(values[flat_row])),
        FlatData::Null(_) => Ok(None),
        _ => Err(ConvertError::Type(col.name)),
    }
}

fn req_i64(col: &Col<'_>, phys_row: usize) -> Result<i64, ConvertError> {
    opt_i64(col, phys_row)?.ok_or(ConvertError::Null(col.name))
}

fn decimal_to_f64(value: i128, scale: i8) -> f64 {
    if scale >= 0 {
        (value as f64) / 10_f64.powi(i32::from(scale))
    } else {
        (value as f64) * 10_f64.powi(-i32::from(scale))
    }
}

fn opt_f64(col: &Col<'_>, phys_row: usize) -> Result<Option<f64>, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::F64(v) => Ok(Some(*v)),
            ScalarValue::Decimal128 { value, scale, .. } => {
                Ok(Some(decimal_to_f64(*value, *scale)))
            }
            ScalarValue::I64(v) => Ok(Some(*v as f64)),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::F64(values) => Ok(Some(values[flat_row])),
        FlatData::Decimal128 { values, scale, .. } => {
            Ok(Some(decimal_to_f64(values[flat_row], *scale)))
        }
        FlatData::I64(values) => Ok(Some(values[flat_row] as f64)),
        FlatData::Null(_) => Ok(None),
        _ => Err(ConvertError::Type(col.name)),
    }
}

fn req_f64(col: &Col<'_>, phys_row: usize) -> Result<f64, ConvertError> {
    opt_f64(col, phys_row)?.ok_or(ConvertError::Null(col.name))
}

fn req_bool(col: &Col<'_>, phys_row: usize) -> Result<bool, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Err(ConvertError::Null(col.name));
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::Bool(b) => Ok(*b),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Err(ConvertError::Null(col.name));
    };
    match fc.data() {
        FlatData::Bool(bm) => Ok(bm.get(flat_row)),
        _ => Err(ConvertError::Type(col.name)),
    }
}

pub(crate) fn opt_time(col: &Col<'_>, phys_row: usize) -> Result<Option<i64>, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::TimestampNs { value, .. } => Ok(Some(*value)),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::TimestampNs { values, .. } => Ok(Some(values[flat_row])),
        FlatData::Null(_) => Ok(None),
        _ => Err(ConvertError::Type(col.name)),
    }
}

fn req_time(col: &Col<'_>, phys_row: usize) -> Result<i64, ConvertError> {
    opt_time(col, phys_row)?.ok_or(ConvertError::Null(col.name))
}

/// A nullable string column parsed with `parse`. A parse failure is `Value(name)`.
fn opt_parsed<T, E>(
    col: &Col<'_>,
    phys_row: usize,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> Result<Option<T>, ConvertError> {
    let Some(text) = read_str(col, phys_row)? else {
        return Ok(None);
    };
    parse(text)
        .map(Some)
        .map_err(|_| ConvertError::Value(col.name))
}

/// A required string column parsed with `parse`. Null is `Null(name)`; a parse failure is `Value(name)`.
fn req_parsed<T, E>(
    col: &Col<'_>,
    phys_row: usize,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> Result<T, ConvertError> {
    let text = read_str(col, phys_row)?.ok_or(ConvertError::Null(col.name))?;
    parse(text).map_err(|_| ConvertError::Value(col.name))
}

/// A `List(Utf8)` row. A null list is empty, and null items are skipped.
fn list_strings(col: &Col<'_>, phys_row: usize) -> Result<Vec<String>, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(Vec::new());
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::List { values, .. } => {
                let mut out = Vec::new();
                for item in values {
                    match item {
                        ScalarValue::Utf8(s) => out.push(s.clone()),
                        ScalarValue::Null(_) => {}
                        _ => return Err(ConvertError::Type(col.name)),
                    }
                }
                Ok(out)
            }
            ScalarValue::Null(_) => Ok(Vec::new()),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(Vec::new());
    };
    match fc.data() {
        FlatData::List {
            offsets, values, ..
        } => {
            let start = *offsets.get(flat_row).ok_or(ConvertError::Value(col.name))? as usize;
            let end = *offsets
                .get(flat_row + 1)
                .ok_or(ConvertError::Value(col.name))? as usize;
            let child = Col {
                column: values.as_ref(),
                name: col.name,
            };
            let mut out = Vec::with_capacity(end.saturating_sub(start));
            for idx in start..end {
                if let Some(item) = opt_string(&child, idx)? {
                    out.push(item);
                }
            }
            Ok(out)
        }
        FlatData::Null(_) => Ok(Vec::new()),
        _ => Err(ConvertError::Type(col.name)),
    }
}

/// A `MapUtf8` row. A null map is empty, and an entry whose value is null is skipped.
fn map_strings(col: &Col<'_>, phys_row: usize) -> Result<BTreeMap<String, String>, ConvertError> {
    let mut out = BTreeMap::new();
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(out);
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::MapUtf8(entries) => {
                for (k, v) in entries {
                    if let Some(val) = v {
                        out.insert(k.clone(), val.clone());
                    }
                }
                Ok(out)
            }
            ScalarValue::Null(_) => Ok(out),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(out);
    };
    match fc.data() {
        FlatData::MapUtf8(map) => {
            let start = *map
                .offsets()
                .get(flat_row)
                .ok_or(ConvertError::Value(col.name))? as usize;
            let end = *map
                .offsets()
                .get(flat_row + 1)
                .ok_or(ConvertError::Value(col.name))? as usize;
            for idx in start..end {
                if map.value_nulls().is_some_and(|bm| !bm.get(idx)) {
                    continue;
                }
                let key = map.keys().get(idx).ok_or(ConvertError::Value(col.name))?;
                let value = map.values().get(idx).ok_or(ConvertError::Value(col.name))?;
                out.insert(key.to_owned(), value.to_owned());
            }
            Ok(out)
        }
        FlatData::Null(_) => Ok(out),
        _ => Err(ConvertError::Type(col.name)),
    }
}

pub(crate) fn event_items(col: &Col<'_>, phys_row: usize) -> Result<Vec<EventItem>, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(Vec::new());
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::Events(items) => Ok(items.clone()),
            ScalarValue::Null(_) => Ok(Vec::new()),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(Vec::new());
    };
    match fc.data() {
        FlatData::Events(events) => events.get(flat_row).ok_or(ConvertError::Value(col.name)),
        FlatData::Null(_) => Ok(Vec::new()),
        _ => Err(ConvertError::Type(col.name)),
    }
}

fn link_items(col: &Col<'_>, phys_row: usize) -> Result<Vec<LinkItem>, ConvertError> {
    if phys_row >= col.column.len() {
        return Err(ConvertError::Value(col.name));
    }
    if col.column.is_null(phys_row) {
        return Ok(Vec::new());
    }
    if let Column::Const { value, .. } = col.column {
        return match value {
            ScalarValue::Links(items) => Ok(items.clone()),
            ScalarValue::Null(_) => Ok(Vec::new()),
            _ => Err(ConvertError::Type(col.name)),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col.column, phys_row) else {
        return Ok(Vec::new());
    };
    match fc.data() {
        FlatData::Links(links) => links.get(flat_row).ok_or(ConvertError::Value(col.name)),
        FlatData::Null(_) => Ok(Vec::new()),
        _ => Err(ConvertError::Type(col.name)),
    }
}

// ---- Converters ----------------------------------------------------------

/// Trace list rows, in row order across batches.
pub(crate) fn trace_summaries(batches: &[Batch]) -> Result<Vec<TraceSummary>, ConvertError> {
    let mut out = Vec::new();
    for batch in batches {
        out.extend(trace_batch(batch)?);
    }
    Ok(out)
}

fn trace_batch(batch: &Batch) -> Result<Vec<TraceSummary>, ConvertError> {
    let trace_id = strings(batch, traces::TRACE_ID)?;
    let root_span_id = strings(batch, traces::ROOT_SPAN_ID)?;
    let has_root = bools(batch, traces::HAS_ROOT)?;
    let name = strings(batch, traces::NAME)?;
    let start_time = times(batch, traces::START_TIME)?;
    let end_time = times(batch, traces::END_TIME)?;
    let duration_ns = ints(batch, TRACE_DURATION_NS)?;
    let status = strings(batch, traces::STATUS)?;
    let session_id = strings(batch, traces::SESSION_ID)?;
    let user_id = strings(batch, traces::USER_ID)?;
    let environment = strings(batch, traces::ENVIRONMENT)?;
    let release = strings(batch, traces::RELEASE)?;
    let tags = lists(batch, traces::TAGS)?;
    let metadata = maps(batch, traces::METADATA)?;
    let input_preview = strings(batch, traces::INPUT_PREVIEW)?;
    let output_preview = strings(batch, traces::OUTPUT_PREVIEW)?;
    let span_count = ints(batch, traces::SPAN_COUNT)?;
    let error_count = ints(batch, traces::ERROR_COUNT)?;
    let llm_count = ints(batch, traces::LLM_COUNT)?;
    let input_tokens = ints(batch, traces::INPUT_TOKENS)?;
    let output_tokens = ints(batch, traces::OUTPUT_TOKENS)?;
    let cache_read_tokens = ints(batch, traces::CACHE_READ_TOKENS)?;
    let cache_write_tokens = ints(batch, traces::CACHE_WRITE_TOKENS)?;
    let reasoning_tokens = ints(batch, traces::REASONING_TOKENS)?;
    let total_tokens = ints(batch, traces::TOTAL_TOKENS)?;
    let cost_total = floats(batch, traces::COST_TOTAL)?;
    let models = lists(batch, traces::MODELS)?;
    let prompts = lists(batch, traces::PROMPTS)?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in active_rows(batch) {
        out.push(TraceSummary {
            trace_id: req_parsed(&trace_id, row, TraceId::from_str)?,
            root_span_id: opt_parsed(&root_span_id, row, SpanId::from_str)?,
            has_root: req_bool(&has_root, row)?,
            name: req_string(&name, row)?,
            start_time: req_time(&start_time, row)?,
            end_time: req_time(&end_time, row)?,
            duration_ns: req_i64(&duration_ns, row)?,
            status: req_parsed(&status, row, SpanStatus::parse)?,
            session_id: opt_string(&session_id, row)?,
            user_id: opt_string(&user_id, row)?,
            environment: opt_string(&environment, row)?,
            release: opt_string(&release, row)?,
            tags: list_strings(&tags, row)?,
            metadata: map_strings(&metadata, row)?,
            input_preview: opt_string(&input_preview, row)?,
            output_preview: opt_string(&output_preview, row)?,
            span_count: req_i64(&span_count, row)?,
            error_count: req_i64(&error_count, row)?,
            llm_count: req_i64(&llm_count, row)?,
            input_tokens: opt_i64(&input_tokens, row)?,
            output_tokens: opt_i64(&output_tokens, row)?,
            cache_read_tokens: opt_i64(&cache_read_tokens, row)?,
            cache_write_tokens: opt_i64(&cache_write_tokens, row)?,
            reasoning_tokens: opt_i64(&reasoning_tokens, row)?,
            total_tokens: opt_i64(&total_tokens, row)?,
            cost_total: opt_f64(&cost_total, row)?,
            models: list_strings(&models, row)?,
            prompts: list_strings(&prompts, row)?,
            scores: Vec::new(),
        });
    }
    Ok(out)
}

/// Span list rows, in row order across batches. Columns are `SPAN_LIST_COLUMNS`.
pub(crate) fn span_summaries(batches: &[Batch]) -> Result<Vec<SpanSummary>, ConvertError> {
    let mut out = Vec::new();
    for batch in batches {
        out.extend(span_batch(batch)?);
    }
    Ok(out)
}

fn span_batch(batch: &Batch) -> Result<Vec<SpanSummary>, ConvertError> {
    let trace_id = strings(batch, spans::TRACE_ID)?;
    let span_id = strings(batch, spans::SPAN_ID)?;
    let parent_span_id = strings(batch, spans::PARENT_SPAN_ID)?;
    let name = strings(batch, spans::NAME)?;
    let span_kind = strings(batch, spans::SPAN_KIND)?;
    let start_time = times(batch, spans::START_TIME)?;
    let end_time = times(batch, spans::END_TIME)?;
    let duration_ns = ints(batch, spans::DURATION_NS)?;
    let status = strings(batch, spans::STATUS)?;
    let status_message = strings(batch, spans::STATUS_MESSAGE)?;
    let service_name = strings(batch, spans::SERVICE_NAME)?;
    let session_id = strings(batch, spans::SESSION_ID)?;
    let user_id = strings(batch, spans::USER_ID)?;
    let environment = strings(batch, spans::ENVIRONMENT)?;
    let release = strings(batch, spans::RELEASE)?;
    let tags = lists(batch, spans::TAGS)?;
    let metadata = maps(batch, spans::METADATA)?;
    let provider = strings(batch, spans::PROVIDER)?;
    // Read only to compute `model`; the stored `model` column is not used.
    let request_model = strings(batch, spans::REQUEST_MODEL)?;
    let response_model = strings(batch, spans::RESPONSE_MODEL)?;
    let input_tokens = ints(batch, spans::INPUT_TOKENS)?;
    let output_tokens = ints(batch, spans::OUTPUT_TOKENS)?;
    let cache_read_tokens = ints(batch, spans::CACHE_READ_TOKENS)?;
    let cache_write_tokens = ints(batch, spans::CACHE_WRITE_TOKENS)?;
    let reasoning_tokens = ints(batch, spans::REASONING_TOKENS)?;
    let total_tokens = ints(batch, spans::TOTAL_TOKENS)?;
    let cost_total = floats(batch, spans::COST_TOTAL)?;
    let ttft_ns = ints(batch, spans::TTFT_NS)?;
    let input_preview = strings(batch, spans::INPUT_PREVIEW)?;
    let output_preview = strings(batch, spans::OUTPUT_PREVIEW)?;
    let prompt_name = strings(batch, spans::PROMPT_NAME)?;
    let prompt_version = ints(batch, spans::PROMPT_VERSION)?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in active_rows(batch) {
        let model = match opt_string(&response_model, row)? {
            Some(m) => Some(m),
            None => opt_string(&request_model, row)?,
        };
        out.push(SpanSummary {
            trace_id: req_parsed(&trace_id, row, TraceId::from_str)?,
            span_id: req_parsed(&span_id, row, SpanId::from_str)?,
            parent_span_id: opt_parsed(&parent_span_id, row, SpanId::from_str)?,
            name: req_string(&name, row)?,
            span_kind: req_parsed(&span_kind, row, SpanKind::parse)?,
            start_time: req_time(&start_time, row)?,
            end_time: req_time(&end_time, row)?,
            duration_ns: req_i64(&duration_ns, row)?,
            status: req_parsed(&status, row, SpanStatus::parse)?,
            status_message: opt_string(&status_message, row)?,
            service_name: opt_string(&service_name, row)?,
            session_id: opt_string(&session_id, row)?,
            user_id: opt_string(&user_id, row)?,
            environment: opt_string(&environment, row)?,
            release: opt_string(&release, row)?,
            tags: list_strings(&tags, row)?,
            metadata: map_strings(&metadata, row)?,
            provider: opt_string(&provider, row)?,
            model,
            prompt_name: opt_string(&prompt_name, row)?,
            prompt_version: opt_i64(&prompt_version, row)?,
            input_tokens: opt_i64(&input_tokens, row)?,
            output_tokens: opt_i64(&output_tokens, row)?,
            cache_read_tokens: opt_i64(&cache_read_tokens, row)?,
            cache_write_tokens: opt_i64(&cache_write_tokens, row)?,
            reasoning_tokens: opt_i64(&reasoning_tokens, row)?,
            total_tokens: opt_i64(&total_tokens, row)?,
            cost_total: opt_f64(&cost_total, row)?,
            ttft_ns: opt_i64(&ttft_ns, row)?,
            input_preview: opt_string(&input_preview, row)?,
            output_preview: opt_string(&output_preview, row)?,
        });
    }
    Ok(out)
}

/// The columns of one batch, extracted once (D-G-14).
struct SpanColumns<'a> {
    trace_id: Col<'a>,
    span_id: Col<'a>,
    parent_span_id: Col<'a>,
    name: Col<'a>,
    span_kind: Col<'a>,
    start_time: Col<'a>,
    end_time: Col<'a>,
    status: Col<'a>,
    status_message: Col<'a>,
    service_name: Col<'a>,
    session_id: Col<'a>,
    user_id: Col<'a>,
    environment: Col<'a>,
    release: Col<'a>,
    tags: Col<'a>,
    metadata: Col<'a>,
    provider: Col<'a>,
    request_model: Col<'a>,
    response_model: Col<'a>,
    prompt_name: Col<'a>,
    prompt_version: Col<'a>,
    operation: Col<'a>,
    input_tokens: Col<'a>,
    output_tokens: Col<'a>,
    cache_read_tokens: Col<'a>,
    cache_write_tokens: Col<'a>,
    reasoning_tokens: Col<'a>,
    total_tokens: Col<'a>,
    cost_input: Col<'a>,
    cost_output: Col<'a>,
    cost_total: Col<'a>,
    cost_source: Col<'a>,
    ttft_ns: Col<'a>,
    request_params: Col<'a>,
    input: Col<'a>,
    input_format: Col<'a>,
    output: Col<'a>,
    output_format: Col<'a>,
    input_preview: Col<'a>,
    output_preview: Col<'a>,
    system_instructions: Col<'a>,
    tool_definitions: Col<'a>,
    attributes: Col<'a>,
    resource_attributes: Col<'a>,
    scope_name: Col<'a>,
    scope_version: Col<'a>,
    events: Col<'a>,
    links: Col<'a>,
    media_refs: Col<'a>,
    truncated: Col<'a>,
    conventions: Col<'a>,
    ingest_time: Col<'a>,
    schema_version: Col<'a>,
}

/// `batch`'s columns, read by name.
fn span_columns<'a>(batch: &'a Batch) -> Result<SpanColumns<'a>, ConvertError> {
    Ok(SpanColumns {
        trace_id: strings(batch, spans::TRACE_ID)?,
        span_id: strings(batch, spans::SPAN_ID)?,
        parent_span_id: strings(batch, spans::PARENT_SPAN_ID)?,
        name: strings(batch, spans::NAME)?,
        span_kind: strings(batch, spans::SPAN_KIND)?,
        start_time: times(batch, spans::START_TIME)?,
        end_time: times(batch, spans::END_TIME)?,
        status: strings(batch, spans::STATUS)?,
        status_message: strings(batch, spans::STATUS_MESSAGE)?,
        service_name: strings(batch, spans::SERVICE_NAME)?,
        session_id: strings(batch, spans::SESSION_ID)?,
        user_id: strings(batch, spans::USER_ID)?,
        environment: strings(batch, spans::ENVIRONMENT)?,
        release: strings(batch, spans::RELEASE)?,
        tags: lists(batch, spans::TAGS)?,
        metadata: maps(batch, spans::METADATA)?,
        provider: strings(batch, spans::PROVIDER)?,
        request_model: strings(batch, spans::REQUEST_MODEL)?,
        response_model: strings(batch, spans::RESPONSE_MODEL)?,
        prompt_name: strings(batch, spans::PROMPT_NAME)?,
        prompt_version: ints(batch, spans::PROMPT_VERSION)?,
        operation: strings(batch, spans::OPERATION)?,
        input_tokens: ints(batch, spans::INPUT_TOKENS)?,
        output_tokens: ints(batch, spans::OUTPUT_TOKENS)?,
        cache_read_tokens: ints(batch, spans::CACHE_READ_TOKENS)?,
        cache_write_tokens: ints(batch, spans::CACHE_WRITE_TOKENS)?,
        reasoning_tokens: ints(batch, spans::REASONING_TOKENS)?,
        total_tokens: ints(batch, spans::TOTAL_TOKENS)?,
        cost_input: floats(batch, spans::COST_INPUT)?,
        cost_output: floats(batch, spans::COST_OUTPUT)?,
        cost_total: floats(batch, spans::COST_TOTAL)?,
        cost_source: strings(batch, spans::COST_SOURCE)?,
        ttft_ns: ints(batch, spans::TTFT_NS)?,
        request_params: strings(batch, spans::REQUEST_PARAMS)?,
        input: strings(batch, spans::INPUT)?,
        input_format: strings(batch, spans::INPUT_FORMAT)?,
        output: strings(batch, spans::OUTPUT)?,
        output_format: strings(batch, spans::OUTPUT_FORMAT)?,
        input_preview: strings(batch, spans::INPUT_PREVIEW)?,
        output_preview: strings(batch, spans::OUTPUT_PREVIEW)?,
        system_instructions: strings(batch, spans::SYSTEM_INSTRUCTIONS)?,
        tool_definitions: strings(batch, spans::TOOL_DEFINITIONS)?,
        attributes: maps(batch, spans::ATTRIBUTES)?,
        resource_attributes: maps(batch, spans::RESOURCE_ATTRIBUTES)?,
        scope_name: strings(batch, spans::SCOPE_NAME)?,
        scope_version: strings(batch, spans::SCOPE_VERSION)?,
        events: events_col(batch, spans::EVENTS)?,
        links: links_col(batch, spans::LINKS)?,
        media_refs: lists(batch, spans::MEDIA_REFS)?,
        truncated: lists(batch, spans::TRUNCATED)?,
        conventions: lists(batch, spans::CONVENTIONS)?,
        ingest_time: times(batch, spans::INGEST_TIME)?,
        schema_version: ints(batch, spans::SCHEMA_VERSION)?,
    })
}

impl SpanColumns<'_> {
    /// Physical row `phys_row` as a span. `project_id` comes from the caller.
    fn row(&self, phys_row: usize, project_id: ProjectId) -> Result<Span, ConvertError> {
        let schema_version = req_i64(&self.schema_version, phys_row)?;
        let schema_version = u16::try_from(schema_version)
            .map_err(|_| ConvertError::Value(spans::SCHEMA_VERSION))?;
        Ok(Span {
            project_id,
            trace_id: req_parsed(&self.trace_id, phys_row, TraceId::from_str)?,
            span_id: req_parsed(&self.span_id, phys_row, SpanId::from_str)?,
            parent_span_id: opt_parsed(&self.parent_span_id, phys_row, SpanId::from_str)?,
            name: req_string(&self.name, phys_row)?,
            span_kind: req_parsed(&self.span_kind, phys_row, SpanKind::parse)?,
            start_time: req_time(&self.start_time, phys_row)?,
            end_time: req_time(&self.end_time, phys_row)?,
            status: req_parsed(&self.status, phys_row, SpanStatus::parse)?,
            status_message: opt_string(&self.status_message, phys_row)?,
            service_name: opt_string(&self.service_name, phys_row)?,
            session_id: opt_string(&self.session_id, phys_row)?,
            user_id: opt_string(&self.user_id, phys_row)?,
            environment: opt_string(&self.environment, phys_row)?,
            release: opt_string(&self.release, phys_row)?,
            tags: list_strings(&self.tags, phys_row)?,
            metadata: map_strings(&self.metadata, phys_row)?,
            provider: opt_string(&self.provider, phys_row)?,
            request_model: opt_string(&self.request_model, phys_row)?,
            response_model: opt_string(&self.response_model, phys_row)?,
            prompt_name: opt_string(&self.prompt_name, phys_row)?,
            prompt_version: opt_i64(&self.prompt_version, phys_row)?,
            operation: opt_string(&self.operation, phys_row)?,
            input_tokens: opt_i64(&self.input_tokens, phys_row)?,
            output_tokens: opt_i64(&self.output_tokens, phys_row)?,
            cache_read_tokens: opt_i64(&self.cache_read_tokens, phys_row)?,
            cache_write_tokens: opt_i64(&self.cache_write_tokens, phys_row)?,
            reasoning_tokens: opt_i64(&self.reasoning_tokens, phys_row)?,
            total_tokens: opt_i64(&self.total_tokens, phys_row)?,
            cost_input: opt_f64(&self.cost_input, phys_row)?,
            cost_output: opt_f64(&self.cost_output, phys_row)?,
            cost_total: opt_f64(&self.cost_total, phys_row)?,
            cost_source: opt_parsed(&self.cost_source, phys_row, CostSource::parse)?,
            ttft_ns: opt_i64(&self.ttft_ns, phys_row)?,
            request_params: opt_string(&self.request_params, phys_row)?,
            input: opt_string(&self.input, phys_row)?,
            input_format: opt_parsed(&self.input_format, phys_row, PayloadFormat::parse)?,
            output: opt_string(&self.output, phys_row)?,
            output_format: opt_parsed(&self.output_format, phys_row, PayloadFormat::parse)?,
            input_preview: opt_string(&self.input_preview, phys_row)?,
            output_preview: opt_string(&self.output_preview, phys_row)?,
            system_instructions: opt_string(&self.system_instructions, phys_row)?,
            tool_definitions: opt_string(&self.tool_definitions, phys_row)?,
            attributes: map_strings(&self.attributes, phys_row)?,
            resource_attributes: map_strings(&self.resource_attributes, phys_row)?,
            scope_name: opt_string(&self.scope_name, phys_row)?,
            scope_version: opt_string(&self.scope_version, phys_row)?,
            events: span_events(&self.events, phys_row)?,
            links: span_links(&self.links, phys_row)?,
            media_refs: list_strings(&self.media_refs, phys_row)?,
            truncated: list_strings(&self.truncated, phys_row)?,
            conventions: span_conventions(&self.conventions, phys_row)?,
            ingest_time: req_time(&self.ingest_time, phys_row)?,
            schema_version,
        })
    }
}

/// The one span of a `span_detail` batch. `project_id` comes from the caller.
///
/// A batch without rows is `Null(span_id)`. Active row 0 is read.
pub(crate) fn span(batch: &Batch, project_id: ProjectId) -> Result<Span, ConvertError> {
    let Some(phys_row) = first_active_row(batch) else {
        return Err(ConvertError::Null(spans::SPAN_ID));
    };
    span_columns(batch)?.row(phys_row, project_id)
}

/// Every row of `batches`, in order. Batches without rows add nothing. Columns are
/// extracted once per batch.
pub(crate) fn spans(batches: &[Batch], project_id: ProjectId) -> Result<Vec<Span>, ConvertError> {
    let mut out = Vec::new();
    for batch in batches.iter().filter(|batch| batch.num_rows() > 0) {
        let columns = span_columns(batch)?;
        for phys_row in active_rows(batch) {
            out.push(columns.row(phys_row, project_id)?);
        }
    }
    Ok(out)
}

/// `events` items: `time`, `name` and a nullable `attributes` JSON text.
fn span_events(col: &Col<'_>, phys_row: usize) -> Result<Vec<SpanEvent>, ConvertError> {
    Ok(event_items(col, phys_row)?
        .into_iter()
        .map(|item| SpanEvent {
            time: item.time,
            name: item.name,
            attributes: item.attributes,
        })
        .collect())
}

/// `links` items: 16-byte `trace_id`, 8-byte `span_id`, and a nullable `attributes` JSON text.
fn span_links(col: &Col<'_>, phys_row: usize) -> Result<Vec<SpanLink>, ConvertError> {
    event_links_convert(link_items(col, phys_row)?)
}

fn event_links_convert(items: Vec<LinkItem>) -> Result<Vec<SpanLink>, ConvertError> {
    items
        .into_iter()
        .map(|item| {
            Ok(SpanLink {
                trace_id: TraceId::from_bytes(item.trace_id)
                    .map_err(|_| ConvertError::Value(spans::LINKS))?,
                span_id: SpanId::from_bytes(item.span_id)
                    .map_err(|_| ConvertError::Value(spans::LINKS))?,
                attributes: item.attributes,
            })
        })
        .collect()
}

/// `conventions` items, each parsed with `Convention::parse`.
fn span_conventions(col: &Col<'_>, phys_row: usize) -> Result<Vec<Convention>, ConvertError> {
    list_strings(col, phys_row)?
        .iter()
        .map(|text| Convention::parse(text).map_err(|_| ConvertError::Value(spans::CONVENTIONS)))
        .collect()
}

/// Session list rows (sessions view), in row order across batches.
pub(crate) fn session_summaries(batches: &[Batch]) -> Result<Vec<SessionSummary>, ConvertError> {
    let mut out = Vec::new();
    for batch in batches {
        out.extend(session_batch(batch)?);
    }
    Ok(out)
}

fn session_batch(batch: &Batch) -> Result<Vec<SessionSummary>, ConvertError> {
    let session_id = strings(batch, traces::SESSION_ID)?;
    let start_time = times(batch, traces::START_TIME)?;
    let end_time = times(batch, traces::END_TIME)?;
    let duration_ns = ints(batch, TRACE_DURATION_NS)?;
    let trace_count = ints(batch, TRACE_COUNT)?;
    let error_count = ints(batch, traces::ERROR_COUNT)?;
    let total_tokens = ints(batch, traces::TOTAL_TOKENS)?;
    let cost_total = floats(batch, traces::COST_TOTAL)?;
    let user_id = strings(batch, traces::USER_ID)?;
    let tags = lists(batch, traces::TAGS)?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in active_rows(batch) {
        out.push(SessionSummary {
            session_id: req_string(&session_id, row)?,
            start_time: req_time(&start_time, row)?,
            end_time: req_time(&end_time, row)?,
            duration_ns: req_i64(&duration_ns, row)?,
            trace_count: req_i64(&trace_count, row)?,
            error_count: req_i64(&error_count, row)?,
            total_tokens: opt_i64(&total_tokens, row)?,
            cost_total: opt_f64(&cost_total, row)?,
            user_id: opt_string(&user_id, row)?,
            tags: list_strings(&tags, row)?,
            scores: Vec::new(),
            // Filled by `session_extras` in the route, which has the extra batches.
            error_trace_count: 0,
            first_input_preview: None,
            last_output_preview: None,
        });
    }
    Ok(out)
}

/// The extras of one session, from [`cetra_storage::session_extras`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionExtra {
    /// Traces of the session whose status is `error`.
    pub(crate) error_trace_count: i64,
    /// `input_preview` of the earliest-starting trace.
    pub(crate) first_input_preview: Option<String>,
    /// `output_preview` of the latest-starting trace.
    pub(crate) last_output_preview: Option<String>,
}

/// Session extras keyed by session id. A session without a row is absent.
pub(crate) fn session_extras(
    batches: &[Batch],
) -> Result<HashMap<String, SessionExtra>, ConvertError> {
    let mut out = HashMap::new();
    for batch in batches {
        let session_id = strings(batch, traces::SESSION_ID)?;
        let error_trace_count = ints(batch, cetra_storage::SESSION_ERROR_TRACE_COUNT)?;
        let first_input_preview = strings(batch, cetra_storage::SESSION_FIRST_INPUT_PREVIEW)?;
        let last_output_preview = strings(batch, cetra_storage::SESSION_LAST_OUTPUT_PREVIEW)?;
        for row in active_rows(batch) {
            let id = req_string(&session_id, row)?;
            let extra = SessionExtra {
                error_trace_count: req_i64(&error_trace_count, row)?,
                first_input_preview: opt_string(&first_input_preview, row)?,
                last_output_preview: opt_string(&last_output_preview, row)?,
            };
            out.insert(id, extra);
        }
    }
    Ok(out)
}

/// End-user list rows (users view), in row order across batches.
pub(crate) fn end_user_summaries(batches: &[Batch]) -> Result<Vec<EndUserSummary>, ConvertError> {
    let mut out = Vec::new();
    for batch in batches {
        out.extend(end_user_batch(batch)?);
    }
    Ok(out)
}

fn end_user_batch(batch: &Batch) -> Result<Vec<EndUserSummary>, ConvertError> {
    let user_id = strings(batch, traces::USER_ID)?;
    let first_seen = times(batch, FIRST_SEEN)?;
    let last_seen = times(batch, LAST_SEEN)?;
    let trace_count = ints(batch, TRACE_COUNT)?;
    let session_count = ints(batch, SESSION_COUNT)?;
    let error_count = ints(batch, traces::ERROR_COUNT)?;
    let error_trace_count = ints(batch, cetra_storage::ERROR_TRACE_COUNT)?;
    let total_tokens = ints(batch, traces::TOTAL_TOKENS)?;
    let cost_total = floats(batch, traces::COST_TOTAL)?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in active_rows(batch) {
        out.push(EndUserSummary {
            user_id: req_string(&user_id, row)?,
            first_seen: req_time(&first_seen, row)?,
            last_seen: req_time(&last_seen, row)?,
            trace_count: req_i64(&trace_count, row)?,
            session_count: req_i64(&session_count, row)?,
            error_count: req_i64(&error_count, row)?,
            error_trace_count: req_i64(&error_trace_count, row)?,
            total_tokens: opt_i64(&total_tokens, row)?,
            cost_total: opt_f64(&cost_total, row)?,
            // Filled by `user_extras` in the route, which has the extra batches.
            activity: Vec::new(),
            scores: Vec::new(),
            metadata: BTreeMap::new(),
        });
    }
    Ok(out)
}

/// The extras of one user, from [`cetra_storage::UserExtras`].
#[derive(Debug, Default)]
pub(crate) struct UserExtra {
    /// [`cetra_storage::ACTIVITY_BUCKETS`] trace counts.
    pub(crate) activity: Vec<i64>,
    /// Score averages, sorted by name then data type.
    pub(crate) scores: Vec<UserScore>,
    /// `user.*` metadata of the latest trace, prefix removed.
    pub(crate) metadata: BTreeMap<String, String>,
}

/// Per-user extras keyed by user id. Buckets outside the activity range are clamped to it.
pub(crate) fn user_extras(
    extras: &cetra_storage::UserExtras,
) -> Result<HashMap<String, UserExtra>, ConvertError> {
    let mut out: HashMap<String, UserExtra> = HashMap::new();
    for batch in &extras.activity {
        let user_id = strings(batch, traces::USER_ID)?;
        let bucket = ints(batch, ACTIVITY_BUCKET)?;
        let trace_count = ints(batch, TRACE_COUNT)?;
        for row in active_rows(batch) {
            let id = req_string(&user_id, row)?;
            let index = req_i64(&bucket, row)?.clamp(
                0,
                i64::try_from(cetra_storage::ACTIVITY_BUCKETS).unwrap_or(i64::MAX) - 1,
            );
            let index = usize::try_from(index).map_err(|_| ConvertError::Value(ACTIVITY_BUCKET))?;
            let entry = out.entry(id).or_default();
            if entry.activity.is_empty() {
                entry.activity = vec![0; cetra_storage::ACTIVITY_BUCKETS];
            }
            entry.activity[index] += req_i64(&trace_count, row)?;
        }
    }
    for batch in &extras.scores {
        let user_id = strings(batch, traces::USER_ID)?;
        let name = strings(batch, scores::NAME)?;
        let data_type = strings(batch, scores::DATA_TYPE)?;
        let avg = floats(batch, SCORE_AVG)?;
        let count = ints(batch, SCORE_COUNT)?;
        for row in active_rows(batch) {
            let id = req_string(&user_id, row)?;
            let data_type = req_parsed(&data_type, row, ScoreDataType::parse)?;
            let score = UserScore {
                name: req_string(&name, row)?,
                data_type,
                avg: req_f64(&avg, row)?,
                count: req_i64(&count, row)?,
            };
            out.entry(id).or_default().scores.push(score);
        }
    }
    for batch in &extras.metadata {
        let user_id = strings(batch, traces::USER_ID)?;
        let metadata = maps(batch, traces::METADATA)?;
        for row in active_rows(batch) {
            let id = req_string(&user_id, row)?;
            let all = map_strings(&metadata, row)?;
            let entry = out.entry(id).or_default();
            entry.metadata = all
                .into_iter()
                .filter_map(|(key, value)| {
                    let key = key.strip_prefix(USER_METADATA_PREFIX)?;
                    (!key.is_empty()).then(|| (key.to_owned(), value))
                })
                .take(cetra_storage::MAX_USER_METADATA_KEYS)
                .collect();
        }
    }
    Ok(out)
}

/// The input and output of a trace's root span, as `(input, output)`.
pub(crate) type RootPayloads = (Option<Payload>, Option<Payload>);

/// The root input and output of each trace, keyed by trace id. A trace without a root has no
/// entry. The first row of a trace id wins.
pub(crate) fn root_payloads(
    batches: &[Batch],
) -> Result<HashMap<TraceId, RootPayloads>, ConvertError> {
    let mut out = HashMap::new();
    for batch in batches {
        let trace_id = strings(batch, spans::TRACE_ID)?;
        let input = strings(batch, spans::INPUT)?;
        let input_format = strings(batch, spans::INPUT_FORMAT)?;
        let output = strings(batch, spans::OUTPUT)?;
        let output_format = strings(batch, spans::OUTPUT_FORMAT)?;
        for row in active_rows(batch) {
            let id = req_parsed(&trace_id, row, TraceId::from_str)?;
            if out.contains_key(&id) {
                continue;
            }
            let input_format = opt_parsed(&input_format, row, PayloadFormat::parse)?;
            let output_format = opt_parsed(&output_format, row, PayloadFormat::parse)?;
            let payloads = (
                stored_payload(input_format, opt_string(&input, row)?),
                stored_payload(output_format, opt_string(&output, row)?),
            );
            out.insert(id, payloads);
        }
    }
    Ok(out)
}

/// Latest-score rows grouped by `target_key`. Each group keeps row order.
pub(crate) fn score_briefs(
    batches: &[Batch],
) -> Result<HashMap<String, Vec<ScoreBrief>>, ConvertError> {
    let mut out: HashMap<String, Vec<ScoreBrief>> = HashMap::new();
    for batch in batches {
        let target_key = strings(batch, TARGET_KEY)?;
        let score_id = strings(batch, scores::SCORE_ID)?;
        let name = strings(batch, scores::NAME)?;
        let data_type = strings(batch, scores::DATA_TYPE)?;
        let value_num = floats(batch, scores::VALUE_NUM)?;
        let value_str = strings(batch, scores::VALUE_STR)?;
        let source = strings(batch, scores::SOURCE)?;
        for row in active_rows(batch) {
            let key = req_string(&target_key, row)?;
            let brief = ScoreBrief {
                score_id: req_parsed(&score_id, row, Uuid::parse_str)?,
                name: req_string(&name, row)?,
                value: score_value(
                    &req_string(&data_type, row)?,
                    opt_f64(&value_num, row)?,
                    opt_string(&value_str, row)?,
                )?,
                source: req_parsed(&source, row, ScoreSource::parse)?,
            };
            out.entry(key).or_default().push(brief);
        }
    }
    Ok(out)
}

/// The API value of one stored score (`data_type`, `value_num`, `value_str`).
pub(crate) fn score_value(
    data_type: &str,
    value_num: Option<f64>,
    value_str: Option<String>,
) -> Result<ScoreValue, ConvertError> {
    let data_type =
        ScoreDataType::parse(data_type).map_err(|_| ConvertError::Value(scores::DATA_TYPE))?;
    match data_type {
        ScoreDataType::Numeric => Ok(ScoreValue::Numeric(
            value_num.ok_or(ConvertError::Null(scores::VALUE_NUM))?,
        )),
        ScoreDataType::Boolean => Ok(ScoreValue::Boolean(
            value_num.ok_or(ConvertError::Null(scores::VALUE_NUM))? != 0.0,
        )),
        ScoreDataType::Categorical => Ok(ScoreValue::Categorical {
            label: value_str.ok_or(ConvertError::Null(scores::VALUE_STR))?,
            value: value_num,
        }),
        ScoreDataType::Text => Ok(ScoreValue::Text(
            value_str.ok_or(ConvertError::Null(scores::VALUE_STR))?,
        )),
    }
}

/// One matched row of a trigger read: the trace, the span for a span rule, and the row's `start_time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MatchRow {
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: Option<SpanId>,
    pub(crate) start_time: i64,
}

/// The rows of `trace_refresh` (`with_span: false`: columns `trace_id`, `start_time`) or
/// `span_refresh` (`with_span: true`: `trace_id`, `span_id`, `start_time`), in row order.
pub(crate) fn match_rows(
    batches: &[Batch],
    with_span: bool,
) -> Result<Vec<MatchRow>, ConvertError> {
    let mut out = Vec::new();
    for batch in batches {
        let (trace_name, time_name) = if with_span {
            (spans::TRACE_ID, spans::START_TIME)
        } else {
            (traces::TRACE_ID, traces::START_TIME)
        };
        let trace_id = strings(batch, trace_name)?;
        let start_time = times(batch, time_name)?;
        let span_id = if with_span {
            Some(strings(batch, spans::SPAN_ID)?)
        } else {
            None
        };
        for row in active_rows(batch) {
            out.push(MatchRow {
                trace_id: req_parsed(&trace_id, row, TraceId::from_str)?,
                span_id: match &span_id {
                    Some(ids) => Some(req_parsed(ids, row, SpanId::from_str)?),
                    None => None,
                },
                start_time: req_time(&start_time, row)?,
            });
        }
    }
    Ok(out)
}

/// Score rows of the scores view, in row order across batches. `project_id` is the caller's project.
pub(crate) fn scores(batches: &[Batch], project_id: ProjectId) -> Result<Vec<Score>, ConvertError> {
    let mut out = Vec::new();
    for batch in batches {
        out.extend(score_batch(batch, project_id)?);
    }
    Ok(out)
}

fn score_batch(batch: &Batch, project_id: ProjectId) -> Result<Vec<Score>, ConvertError> {
    let score_id = strings(batch, scores::SCORE_ID)?;
    let target = strings(batch, scores::TARGET)?;
    let trace_id = strings(batch, scores::TRACE_ID)?;
    let span_id = strings(batch, scores::SPAN_ID)?;
    let session_id = strings(batch, scores::SESSION_ID)?;
    let user_id = strings(batch, scores::USER_ID)?;
    let name = strings(batch, scores::NAME)?;
    let data_type = strings(batch, scores::DATA_TYPE)?;
    let value_num = floats(batch, scores::VALUE_NUM)?;
    let value_str = strings(batch, scores::VALUE_STR)?;
    let reason = strings(batch, scores::REASON)?;
    let source = strings(batch, scores::SOURCE)?;
    let evaluator_id = strings(batch, scores::EVALUATOR_ID)?;
    let rule_id = strings(batch, scores::RULE_ID)?;
    let experiment_id = strings(batch, scores::EXPERIMENT_ID)?;
    let dataset_item_id = strings(batch, scores::DATASET_ITEM_ID)?;
    let queue_id = strings(batch, scores::QUEUE_ID)?;
    let author_user_id = strings(batch, scores::AUTHOR_USER_ID)?;
    let metadata = maps(batch, scores::METADATA)?;
    let created_at = times(batch, scores::CREATED_AT)?;
    let updated_at = times(batch, scores::UPDATED_AT)?;
    let deleted = bools(batch, scores::DELETED)?;
    let ingest_time = times(batch, scores::INGEST_TIME)?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in active_rows(batch) {
        let kind = req_parsed(&target, row, ScoreTargetKind::parse)?;
        let (score_target, score_session) = match kind {
            ScoreTargetKind::Span => (
                ScoreTarget::Span {
                    trace_id: req_parsed(&trace_id, row, TraceId::from_str)?,
                    span_id: req_parsed(&span_id, row, SpanId::from_str)?,
                },
                opt_string(&session_id, row)?,
            ),
            ScoreTargetKind::Trace => (
                ScoreTarget::Trace {
                    trace_id: req_parsed(&trace_id, row, TraceId::from_str)?,
                },
                opt_string(&session_id, row)?,
            ),
            // The target's id is stored in `session_id`, so the score has no other session.
            ScoreTargetKind::Session => (
                ScoreTarget::Session {
                    session_id: req_string(&session_id, row)?,
                },
                None,
            ),
        };
        out.push(Score {
            project_id,
            score_id: req_parsed(&score_id, row, Uuid::parse_str)?,
            target: score_target,
            session_id: score_session,
            user_id: opt_string(&user_id, row)?,
            name: req_string(&name, row)?,
            value: score_value(
                &req_string(&data_type, row)?,
                opt_f64(&value_num, row)?,
                opt_string(&value_str, row)?,
            )?,
            reason: opt_string(&reason, row)?,
            source: req_parsed(&source, row, ScoreSource::parse)?,
            evaluator_id: opt_parsed(&evaluator_id, row, Uuid::parse_str)?,
            rule_id: opt_parsed(&rule_id, row, Uuid::parse_str)?,
            experiment_id: opt_parsed(&experiment_id, row, Uuid::parse_str)?,
            dataset_item_id: opt_parsed(&dataset_item_id, row, Uuid::parse_str)?,
            queue_id: opt_parsed(&queue_id, row, Uuid::parse_str)?,
            author_user_id: opt_parsed(&author_user_id, row, Uuid::parse_str)?,
            metadata: map_strings(&metadata, row)?,
            created_at: req_time(&created_at, row)?,
            updated_at: req_time(&updated_at, row)?,
            deleted: req_bool(&deleted, row)?,
            ingest_time: req_time(&ingest_time, row)?,
        });
    }
    Ok(out)
}

/// The API widget answer. Storage names map variant by variant.
pub(crate) fn widget_response(data: cetra_storage::WidgetData) -> WidgetQueryResponse {
    match data {
        cetra_storage::WidgetData::Series { bucket_ns, series } => {
            WidgetQueryResponse::Series(WidgetSeriesResponse {
                bucket_ns,
                series: series
                    .into_iter()
                    .map(|series| WidgetSeries {
                        measure: series.measure,
                        group: series.group,
                        other: series.other,
                        points: series
                            .points
                            .into_iter()
                            .map(|point| WidgetPoint {
                                time: point.time,
                                value: point.value,
                                rows: point.rows,
                            })
                            .collect(),
                    })
                    .collect(),
            })
        }
        cetra_storage::WidgetData::Total { rows } => {
            WidgetQueryResponse::Total(WidgetTotalResponse {
                rows: rows
                    .into_iter()
                    .map(|row| TotalRow {
                        group: row.group,
                        other: row.other,
                        values: row.values,
                        previous: row.previous,
                    })
                    .collect(),
            })
        }
    }
}

/// The API catalog of views and functions. Storage names map variant by variant.
pub(crate) fn schema_response() -> SchemaResponse {
    SchemaResponse {
        views: cetra_storage::view_catalog()
            .into_iter()
            .chain(crate::datasets::sql_views::catalog())
            .map(|view| ViewSchema {
                name: view_name(view.view),
                description: view.description.to_owned(),
                columns: view
                    .columns
                    .into_iter()
                    .map(|column| ColumnSchema {
                        name: column.name.to_owned(),
                        column_type: column_type(column.kind),
                        nullable: column.nullable,
                        description: column.description.to_owned(),
                    })
                    .collect(),
            })
            .collect(),
        functions: cetra_storage::function_catalog()
            .into_iter()
            .map(|function| FunctionSchema {
                name: function.name.to_owned(),
                signature: function.signature.to_owned(),
                description: function.description.to_owned(),
                in_filter: function.in_filter,
                in_sql: function.in_sql,
            })
            .collect(),
    }
}

fn view_name(name: cetra_storage::ViewName) -> super::schema::ViewName {
    use super::schema::ViewName as Api;
    use cetra_storage::ViewName;
    match name {
        ViewName::Traces => Api::Traces,
        ViewName::Spans => Api::Spans,
        ViewName::Sessions => Api::Sessions,
        ViewName::Scores => Api::Scores,
        ViewName::ExperimentItems => Api::ExperimentItems,
        ViewName::DatasetItems => Api::DatasetItems,
    }
}

fn column_type(kind: cetra_storage::ColumnKind) -> ColumnType {
    use cetra_storage::ColumnKind;
    match kind {
        ColumnKind::String => ColumnType::String,
        ColumnKind::Integer => ColumnType::Integer,
        ColumnKind::Float => ColumnType::Float,
        ColumnKind::Boolean => ColumnType::Boolean,
        ColumnKind::Timestamp => ColumnType::Timestamp,
        ColumnKind::StringList => ColumnType::StringList,
        ColumnKind::StringMap => ColumnType::StringMap,
        ColumnKind::Json => ColumnType::Json,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{
        DictColumn, EventsData, Field, LinksData, MapData, RleColumn, Schema, Utf8Data,
    };
    use cetra_storage::{
        QueryContext, ScoreTargets, SortOrder as StorageSortOrder, SpanListRequest, SpanSort,
        TraceListRequest, TraceSort,
    };

    use super::*;
    use crate::routes::read::{READ_TIMEOUT, list_scope, open_query};
    use crate::test_support::TestApp;

    /// A project with the example dataset loaded. The app is returned so it outlives the context.
    async fn example_context() -> (TestApp, QueryContext) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        let ctx = open_query(&app.state, seeded.org_id, seeded.project.id, None)
            .await
            .unwrap();
        (app, ctx)
    }

    async fn trace_rows(ctx: &QueryContext, include_eval: bool) -> Vec<TraceSummary> {
        let page = cetra_storage::trace_list(
            ctx,
            TraceListRequest {
                filter: None,
                search: None,
                strict_exclusions: false,
                scope: list_scope(None, None, include_eval, false).unwrap(),
                sort: TraceSort::StartTime,
                order: StorageSortOrder::Desc,
                limit: 50,
                cursor: None,
                timeout: READ_TIMEOUT,
            },
        )
        .await
        .unwrap();
        trace_summaries(&page.batches).unwrap()
    }

    async fn span_page(ctx: &QueryContext) -> cetra_storage::Page {
        cetra_storage::span_list(
            ctx,
            SpanListRequest {
                filter: None,
                search: None,
                strict_exclusions: false,
                scope: list_scope(None, None, false, false).unwrap(),
                sort: SpanSort::StartTime,
                order: StorageSortOrder::Desc,
                limit: 50,
                cursor: None,
                timeout: READ_TIMEOUT,
            },
        )
        .await
        .unwrap()
    }

    fn utf8_col(values: &[&str]) -> Column {
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(values.iter().copied()).unwrap()),
                None,
            )
            .unwrap(),
        )
    }

    fn ts_col(values: Vec<i64>) -> Column {
        Column::Flat(
            FlatColumn::try_new(FlatData::TimestampNs { values, utc: true }, None).unwrap(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trace_rows_match_the_dataset() {
        let (_app, ctx) = example_context().await;
        let traces = trace_rows(&ctx, true).await;
        let ids: Vec<String> = traces.iter().map(|t| t.trace_id.to_string()).collect();
        assert_eq!(
            ids,
            [
                "a1000000000000000000000000000006",
                "a1000000000000000000000000000004",
                "a1000000000000000000000000000003",
                "a1000000000000000000000000000002",
                "a1000000000000000000000000000005",
                "a1000000000000000000000000000001",
            ]
        );

        let trace1 = &traces[5];
        assert!(trace1.has_root);
        assert_eq!(
            trace1
                .root_span_id
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("b100000000000011")
        );
        assert_eq!(trace1.span_count, 4);
        assert_eq!(trace1.tags, ["beta"]);
        assert!((trace1.cost_total.unwrap() - 0.00306).abs() < 1e-12);
        assert_eq!(trace1.status, SpanStatus::Ok);
        assert!(trace1.scores.is_empty());

        let trace6 = &traces[0];
        assert!(!trace6.has_root);
        assert!(trace6.root_span_id.is_none());

        let trace3 = &traces[2];
        assert_eq!(trace3.status, SpanStatus::Error);
        assert_eq!(trace3.error_count, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn span_rows_match_the_dataset() {
        let (_app, ctx) = example_context().await;
        let spans = span_summaries(&span_page(&ctx).await.batches).unwrap();
        assert_eq!(spans.len(), 12);
        assert_eq!(spans[0].span_id.to_string(), "b100000000000061");
        let span12 = spans
            .iter()
            .find(|span| span.span_id.to_string() == "b100000000000012")
            .unwrap();
        assert_eq!(span12.model.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn latest_score_briefs_group_by_target() {
        let (_app, ctx) = example_context().await;
        let targets = [
            "a1000000000000000000000000000001",
            "a1000000000000000000000000000002",
            "a1000000000000000000000000000003",
        ];
        let batches =
            cetra_storage::latest_scores(&ctx, ScoreTargets::Traces(&targets), READ_TIMEOUT)
                .await
                .unwrap();
        let briefs = score_briefs(&batches).unwrap();

        let trace1 = &briefs[targets[0]];
        assert_eq!(trace1.len(), 1);
        assert_eq!(trace1[0].name, "helpfulness");
        assert_eq!(trace1[0].value, ScoreValue::Numeric(0.9));
        assert_eq!(trace1[0].source, ScoreSource::LlmJudge);
        assert_eq!(
            trace1[0].score_id,
            Uuid::parse_str("01900000-0000-7000-8000-000000000302").unwrap()
        );
        assert_eq!(briefs[targets[1]][0].value, ScoreValue::Boolean(true));
        assert_eq!(
            briefs[targets[2]][0].value,
            ScoreValue::Text("Needs retry with backoff.".to_owned())
        );
    }

    #[test]
    fn score_value_maps_each_data_type() {
        assert_eq!(
            score_value("numeric", Some(0.5), None).unwrap(),
            ScoreValue::Numeric(0.5)
        );
        assert_eq!(
            score_value("boolean", Some(0.0), None).unwrap(),
            ScoreValue::Boolean(false)
        );
        assert_eq!(
            score_value("categorical", Some(1.0), Some("friendly".to_owned())).unwrap(),
            ScoreValue::Categorical {
                label: "friendly".to_owned(),
                value: Some(1.0),
            }
        );
        assert_eq!(
            score_value("text", None, Some("note".to_owned())).unwrap(),
            ScoreValue::Text("note".to_owned())
        );
    }

    #[test]
    fn score_value_names_the_missing_column() {
        assert!(matches!(
            score_value("numeric", None, None),
            Err(ConvertError::Null("value_num"))
        ));
        assert!(matches!(
            score_value("text", None, None),
            Err(ConvertError::Null("value_str"))
        ));
        assert!(matches!(
            score_value("unknown", None, None),
            Err(ConvertError::Value("data_type"))
        ));
    }

    #[test]
    fn a_missing_column_is_reported_by_name() {
        let schema =
            Schema::try_new_arc(vec![Field::new(traces::TRACE_ID, DataType::Utf8, false)]).unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![utf8_col(&["a1000000000000000000000000000001"])],
            1,
        )
        .unwrap();
        let error = trace_summaries(&[batch]).unwrap_err();
        assert!(matches!(error, ConvertError::Missing(_)));
    }

    #[test]
    fn match_rows_reads_trace_and_span_batches() {
        let trace_hex = "a1000000000000000000000000000001";
        let span_hex = "0000000000000001";

        let trace_schema = Schema::try_new_arc(vec![
            Field::new(traces::TRACE_ID, DataType::Utf8, false),
            Field::new(traces::START_TIME, DataType::timestamp_utc(), false),
        ])
        .unwrap();
        let trace_batch = Batch::try_new_all(
            trace_schema,
            vec![utf8_col(&[trace_hex]), ts_col(vec![7])],
            1,
        )
        .unwrap();
        let rows = match_rows(&[trace_batch], false).unwrap();
        let expected = MatchRow {
            trace_id: trace_hex.parse().unwrap(),
            span_id: None,
            start_time: 7,
        };
        assert_eq!(rows, vec![expected]);

        let span_schema = Schema::try_new_arc(vec![
            Field::new(spans::TRACE_ID, DataType::Utf8, false),
            Field::new(spans::SPAN_ID, DataType::Utf8, false),
            Field::new(spans::START_TIME, DataType::timestamp_utc(), false),
        ])
        .unwrap();
        let span_batch = Batch::try_new_all(
            span_schema,
            vec![
                utf8_col(&[trace_hex]),
                utf8_col(&[span_hex]),
                ts_col(vec![7]),
            ],
            1,
        )
        .unwrap();
        let rows = match_rows(std::slice::from_ref(&span_batch), true).unwrap();
        assert_eq!(rows[0].span_id, Some(span_hex.parse().unwrap()));

        // A span batch without its span id column does not convert.
        let without_span_schema = Schema::try_new_arc(vec![
            Field::new(spans::TRACE_ID, DataType::Utf8, false),
            Field::new(spans::START_TIME, DataType::timestamp_utc(), false),
        ])
        .unwrap();
        let without_span = Batch::try_new_all(
            without_span_schema,
            vec![utf8_col(&[trace_hex]), ts_col(vec![7])],
            1,
        )
        .unwrap();
        assert!(match_rows(&[without_span], true).is_err());
    }

    #[test]
    fn readers_handle_all_encodings_and_selections() {
        let trace_hex = "a1000000000000000000000000000001";
        let span_hex = "b100000000000001";
        let project_id = ProjectId::from_uuid(Uuid::now_v7());

        // Dict Utf8 column with null at row 0.
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["unused", "row-b", "row-c"]).unwrap()),
            None,
        )
        .unwrap();
        let mut dict_nulls = Bitmap::new_ones(3);
        let _ = dict_nulls.set(0, false);
        let dict_col =
            Column::Dict(DictColumn::try_new(dict_vals, vec![0, 1, 2], Some(dict_nulls)).unwrap());

        // Rle I64 column.
        let rle_vals = FlatColumn::try_new(FlatData::I64(vec![10, 42]), None).unwrap();
        let rle_col = Column::Rle(RleColumn::try_new(rle_vals, vec![1, 3]).unwrap());

        // Const Bool column.
        let const_bool = Column::try_new_const(ScalarValue::Bool(true), 3).unwrap();

        // Dict List(Utf8) column with nested Dict child values.
        let inner_dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
            None,
        )
        .unwrap();
        let inner_dict =
            Column::Dict(DictColumn::try_new(inner_dict_vals, vec![0, 1, 1], None).unwrap());
        let list_flat = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: vec![0, 1, 3, 3],
                values: Box::new(inner_dict),
            },
            None,
        )
        .unwrap();
        let list_col = Column::Flat(list_flat);

        // MapUtf8 column with a null entry value.
        let mut val_nulls = Bitmap::new_ones(2);
        let _ = val_nulls.set(1, false);
        let map_data = MapData::try_new(
            vec![0, 0, 2, 2],
            Utf8Data::try_from_slices(["k1", "k2"]).unwrap(),
            Utf8Data::try_from_slices(["v1", ""]).unwrap(),
            Some(val_nulls),
        )
        .unwrap();
        let map_col =
            Column::Flat(FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), None).unwrap());

        // Events & Links columns.
        let events_data = EventsData::try_new(
            vec![0, 0, 1, 1],
            vec![999],
            Utf8Data::try_from_slices(["evt"]).unwrap(),
            Utf8Data::try_from_slices(["{\"a\":1}"]).unwrap(),
            None,
        )
        .unwrap();
        let events_col_val = Column::Flat(
            FlatColumn::try_new(FlatData::Events(Box::new(events_data)), None).unwrap(),
        );

        let links_data = LinksData::try_new(
            vec![0, 0, 1, 1],
            vec![[1_u8; 16]],
            vec![[2_u8; 8]],
            Utf8Data::try_from_slices([""]).unwrap(),
            Some(Bitmap::new_zeroed(1)),
        )
        .unwrap();
        let links_col_val =
            Column::Flat(FlatColumn::try_new(FlatData::Links(Box::new(links_data)), None).unwrap());

        let schema = Schema::try_new_arc(vec![
            Field::new(traces::TRACE_ID, DataType::Utf8, false),
            Field::new(spans::SPAN_ID, DataType::Utf8, false),
            Field::new(spans::NAME, DataType::Utf8, true),
            Field::new(spans::SCHEMA_VERSION, DataType::I64, false),
            Field::new(spans::START_TIME, DataType::timestamp_utc(), false),
            Field::new("flag", DataType::Bool, false),
            Field::new(spans::TAGS, DataType::List(Box::new(DataType::Utf8)), false),
            Field::new(spans::METADATA, DataType::MapUtf8, false),
            Field::new(spans::EVENTS, DataType::Events, false),
            Field::new(spans::LINKS, DataType::Links, false),
        ])
        .unwrap();

        let cols = vec![
            Column::try_new_const(ScalarValue::Utf8(trace_hex.to_owned()), 3).unwrap(),
            Column::try_new_const(ScalarValue::Utf8(span_hex.to_owned()), 3).unwrap(),
            dict_col,
            rle_col,
            ts_col(vec![100, 200, 300]),
            const_bool,
            list_col,
            map_col,
            events_col_val,
            links_col_val,
        ];

        // Mask selecting physical row 1 only.
        let mut mask = Bitmap::new_zeroed(3);
        let _ = mask.set(1, true);
        let masked_batch =
            Batch::try_new(Arc::clone(&schema), cols.clone(), 3, Selection::Mask(mask)).unwrap();
        assert_eq!(first_active_row(&masked_batch), Some(1));
        assert_eq!(
            req_string(&strings(&masked_batch, spans::NAME).unwrap(), 1).unwrap(),
            "row-b"
        );
        assert_eq!(
            req_i64(&ints(&masked_batch, spans::SCHEMA_VERSION).unwrap(), 1).unwrap(),
            42
        );
        assert!(req_bool(&bools(&masked_batch, "flag").unwrap(), 1).unwrap());
        assert_eq!(
            list_strings(&lists(&masked_batch, spans::TAGS).unwrap(), 1).unwrap(),
            vec!["beta", "beta"]
        );
        assert_eq!(
            map_strings(&maps(&masked_batch, spans::METADATA).unwrap(), 1).unwrap(),
            BTreeMap::from([("k1".to_owned(), "v1".to_owned())])
        );
        let evs = span_events(&events_col(&masked_batch, spans::EVENTS).unwrap(), 1).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].name, "evt");
        let lnks = span_links(&links_col(&masked_batch, spans::LINKS).unwrap(), 1).unwrap();
        assert_eq!(lnks.len(), 1);
        assert_eq!(lnks[0].attributes, None);

        // Indices selecting physical rows [2, 1].
        let idx_batch = Batch::try_new(schema, cols, 3, Selection::Indices(vec![2, 1])).unwrap();
        let matched = match_rows(&[idx_batch], true).unwrap();
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].start_time, 300);
        assert_eq!(matched[1].start_time, 200);

        // Missing required columns fail conversion cleanly.
        let bad_schema = Schema::try_new_arc(vec![Field::new(
            spans::SCHEMA_VERSION,
            DataType::I64,
            false,
        )])
        .unwrap();
        let bad_batch = Batch::try_new_all(
            bad_schema,
            vec![Column::try_new_const(ScalarValue::I64(i64::from(u16::MAX) + 1), 1).unwrap()],
            1,
        )
        .unwrap();
        assert!(matches!(
            span(&bad_batch, project_id),
            Err(ConvertError::Missing(spans::TRACE_ID))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn out_of_range_schema_version_fails_with_value_error() {
        let (_app, ctx) = example_context().await;
        let project_id = ProjectId::from_uuid(Uuid::now_v7());
        let base = cetra_storage::span_detail(
            &ctx,
            "a1000000000000000000000000000001",
            "b100000000000012",
            READ_TIMEOUT,
        )
        .await
        .unwrap()
        .expect("example dataset has span b100000000000012");
        let sv_idx = base
            .schema()
            .index_of(spans::SCHEMA_VERSION)
            .expect("schema_version column");
        let raw_len = base.raw_len();
        for bad_version in [i64::from(u16::MAX) + 1, -1] {
            let mut cols = base.columns().to_vec();
            cols[sv_idx] =
                Arc::new(Column::try_new_const(ScalarValue::I64(bad_version), raw_len).unwrap());
            let bad_batch = Batch::try_new_shared(
                Arc::clone(base.schema()),
                cols,
                raw_len,
                base.selection().clone(),
            )
            .unwrap();
            assert!(
                matches!(
                    span(&bad_batch, project_id),
                    Err(ConvertError::Value(spans::SCHEMA_VERSION))
                ),
                "expected ConvertError::Value(schema_version) for {bad_version}"
            );
        }
    }
}
