//! Trace partials and the merge specification (spec 01 § Trace, D152).
//!
//! [`partials_from_spans_batch`] builds one partial per trace of a spans batch.
//! [`MergeColumns`] defines the closed column set that `SortedTraceMerger`
//! accumulates; there is no SQL `GROUP BY trace_id` in storage code.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use cetra_column::{
    Batch, Bitmap, Column as Col, DataType as ColType, FlatColumn, FlatData, MapData, ScalarValue,
    Selection, Utf8Data,
};
use cetra_core::enums::{SpanKind, SpanStatus};
use cetra_core::ids::TraceId;
use cetra_core::schema::{spans, traces};

use crate::error::PartialsError;

/// The columns of `traces_schema()`, in schema order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Column {
    ProjectId,
    TraceId,
    StartTime,
    EndTime,
    Name,
    RootSpanId,
    HasRoot,
    Status,
    SessionId,
    UserId,
    Environment,
    Release,
    Tags,
    Metadata,
    InputPreview,
    OutputPreview,
    SpanCount,
    ErrorCount,
    LlmCount,
    InputTokens,
    OutputTokens,
    CacheReadTokens,
    CacheWriteTokens,
    ReasoningTokens,
    TotalTokens,
    CostTotal,
    Models,
    Prompts,
    LastIngestTime,
    PartialCount,
}

impl Column {
    const ALL: [Self; 30] = [
        Self::ProjectId,
        Self::TraceId,
        Self::StartTime,
        Self::EndTime,
        Self::Name,
        Self::RootSpanId,
        Self::HasRoot,
        Self::Status,
        Self::SessionId,
        Self::UserId,
        Self::Environment,
        Self::Release,
        Self::Tags,
        Self::Metadata,
        Self::InputPreview,
        Self::OutputPreview,
        Self::SpanCount,
        Self::ErrorCount,
        Self::LlmCount,
        Self::InputTokens,
        Self::OutputTokens,
        Self::CacheReadTokens,
        Self::CacheWriteTokens,
        Self::ReasoningTokens,
        Self::TotalTokens,
        Self::CostTotal,
        Self::Models,
        Self::Prompts,
        Self::LastIngestTime,
        Self::PartialCount,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::ProjectId => traces::PROJECT_ID,
            Self::TraceId => traces::TRACE_ID,
            Self::StartTime => traces::START_TIME,
            Self::EndTime => traces::END_TIME,
            Self::Name => traces::NAME,
            Self::RootSpanId => traces::ROOT_SPAN_ID,
            Self::HasRoot => traces::HAS_ROOT,
            Self::Status => traces::STATUS,
            Self::SessionId => traces::SESSION_ID,
            Self::UserId => traces::USER_ID,
            Self::Environment => traces::ENVIRONMENT,
            Self::Release => traces::RELEASE,
            Self::Tags => traces::TAGS,
            Self::Metadata => traces::METADATA,
            Self::InputPreview => traces::INPUT_PREVIEW,
            Self::OutputPreview => traces::OUTPUT_PREVIEW,
            Self::SpanCount => traces::SPAN_COUNT,
            Self::ErrorCount => traces::ERROR_COUNT,
            Self::LlmCount => traces::LLM_COUNT,
            Self::InputTokens => traces::INPUT_TOKENS,
            Self::OutputTokens => traces::OUTPUT_TOKENS,
            Self::CacheReadTokens => traces::CACHE_READ_TOKENS,
            Self::CacheWriteTokens => traces::CACHE_WRITE_TOKENS,
            Self::ReasoningTokens => traces::REASONING_TOKENS,
            Self::TotalTokens => traces::TOTAL_TOKENS,
            Self::CostTotal => traces::COST_TOTAL,
            Self::Models => traces::MODELS,
            Self::Prompts => traces::PROMPTS,
            Self::LastIngestTime => traces::LAST_INGEST_TIME,
            Self::PartialCount => traces::PARTIAL_COUNT,
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|column| column.name() == name)
    }
}

/// The closed set of columns a merge reads and produces (D152).
///
/// `output()` is the requested set plus `trace_id` and `start_time`. `input()`
/// adds `status`'s dependency `error_count` and the four merge-order keys. Both
/// are in `traces_schema()` order. A merger never reads or builds another column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeColumns {
    output: Vec<Column>,
    input: Vec<Column>,
}

impl MergeColumns {
    /// Names not in `traces_schema()` are ignored; they cannot be read or built.
    pub(crate) fn new(requested: &[&str]) -> Self {
        let mut set = BTreeSet::new();
        for name in requested {
            if let Some(column) = Column::from_name(name) {
                set.insert(column);
            }
        }
        set.insert(Column::TraceId);
        set.insert(Column::StartTime);
        let output: Vec<Column> = set.iter().copied().collect();

        let mut input_set = set;
        if input_set.contains(&Column::Status) {
            input_set.insert(Column::ErrorCount);
        }
        for key in [
            Column::TraceId,
            Column::StartTime,
            Column::HasRoot,
            Column::RootSpanId,
        ] {
            input_set.insert(key);
        }
        let input: Vec<Column> = input_set.iter().copied().collect();
        Self { output, input }
    }

    /// Columns the merged batch carries, in `traces_schema()` order.
    pub(crate) fn output(&self) -> Vec<&'static str> {
        self.output.iter().map(|column| column.name()).collect()
    }

    /// Columns a partial batch must carry, in `traces_schema()` order.
    pub(crate) fn input(&self) -> Vec<&'static str> {
        self.input.iter().map(|column| column.name()).collect()
    }

    #[cfg(test)]
    pub(crate) fn all() -> Self {
        Self {
            output: Column::ALL.to_vec(),
            input: Column::ALL.to_vec(),
        }
    }
}

/// A partial's position in merge order. Smaller is earlier (spec 01 § Order).
///
/// A root candidate (`root = 0`) comes first; among those, the smallest
/// `(start_time, root_span_id)` is the root partial. The other partials follow
/// in `start_time` order, with `tie = None` so their values break ties. Span
/// rows use `tie = Some(span_id)`, so they follow `(start_time, span_id)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    root: u8,
    start_time: i64,
    tie: Option<Vec<u8>>,
}

/// A value together with the partial that contributed it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Ranked<T> {
    rank: Rank,
    value: T,
}

/// One cell read from an input batch. Owned so readers can borrow the batch.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Null,
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    I64(i64),
    I32(i32),
    F64(f64),
    StrList(Vec<String>),
    StrMap(Vec<(String, String)>),
}

/// One accumulated column. Strings are owned; sets and maps stay sorted until
/// `finish`.
#[derive(Debug, Clone, PartialEq)]
enum Acc {
    Bytes(Vec<u8>),
    I64(i64),
    I32(i32),
    /// USD in units of 1e-18, summed exactly so the merge order does not matter (D-A2-4).
    Usd(i128),
    Bool(bool),
    RankedStr(Ranked<String>),
    RankedBytes(Ranked<Vec<u8>>),
    StrSet(BTreeSet<String>),
    StrMap(BTreeMap<String, Ranked<String>>),
}

/// Per-row facts the accumulation rules need beyond the value itself.
struct RowCtx {
    rank: Rank,
    /// A root candidate (null `parent_span_id` at span level, `has_root` at merge level).
    root: bool,
    /// May contribute a preview (root or `llm` at span level, every partial at merge level).
    preview: bool,
}

/// One Option per column, shared by both entry points.
struct TraceAcc {
    values: Vec<Option<Acc>>,
}

impl TraceAcc {
    fn new() -> Self {
        Self {
            values: Column::ALL.iter().map(|_| None).collect(),
        }
    }

    fn slot(&mut self, column: Column) -> &mut Option<Acc> {
        &mut self.values[column.index()]
    }

    /// Applies one column's rule. Returns the heap bytes this added, for `size()`.
    fn update(&mut self, column: Column, val: Val, ctx: &RowCtx) -> usize {
        if val == Val::Null {
            return 0;
        }
        match column {
            Column::TraceId => set_bytes(self.slot(column), val),
            Column::ProjectId
            | Column::Name
            | Column::SessionId
            | Column::UserId
            | Column::Environment
            | Column::Release => ranked_str(self.slot(column), val, ctx),
            Column::InputPreview | Column::OutputPreview => {
                if ctx.preview {
                    ranked_str(self.slot(column), val, ctx)
                } else {
                    0
                }
            }
            Column::Status => {
                if ctx.root {
                    ranked_str(self.slot(column), val, ctx)
                } else {
                    0
                }
            }
            Column::RootSpanId => {
                if ctx.root {
                    ranked_bytes(self.slot(column), val, ctx)
                } else {
                    0
                }
            }
            Column::HasRoot => {
                let Val::Bool(value) = val else { return 0 };
                let slot = self.slot(column);
                match slot {
                    Some(Acc::Bool(kept)) => *kept |= value,
                    _ => *slot = Some(Acc::Bool(value)),
                }
                0
            }
            Column::StartTime => min_i64(self.slot(column), &val),
            Column::EndTime | Column::LastIngestTime => max_i64(self.slot(column), &val),
            Column::Tags | Column::Models | Column::Prompts => str_set(self.slot(column), val),
            Column::Metadata => str_map(self.slot(column), val, ctx),
            Column::SpanCount
            | Column::ErrorCount
            | Column::LlmCount
            | Column::InputTokens
            | Column::OutputTokens
            | Column::CacheReadTokens
            | Column::CacheWriteTokens
            | Column::ReasoningTokens
            | Column::TotalTokens => sum_i64(self.slot(column), &val),
            Column::CostTotal => sum_usd(self.slot(column), &val),
            Column::PartialCount => sum_i32(self.slot(column), &val),
        }
    }
}

fn set_bytes(slot: &mut Option<Acc>, val: Val) -> usize {
    let Val::Bytes(bytes) = val else { return 0 };
    let grown = bytes.len();
    *slot = Some(Acc::Bytes(bytes));
    grown
}

fn ranked_str(slot: &mut Option<Acc>, val: Val, ctx: &RowCtx) -> usize {
    let Val::Str(text) = val else { return 0 };
    let grown = text.len();
    let candidate = Ranked {
        rank: ctx.rank.clone(),
        value: text,
    };
    match slot {
        Some(Acc::RankedStr(kept)) => {
            if candidate < *kept {
                *kept = candidate;
            }
        }
        _ => *slot = Some(Acc::RankedStr(candidate)),
    }
    grown
}

fn ranked_bytes(slot: &mut Option<Acc>, val: Val, ctx: &RowCtx) -> usize {
    let Val::Bytes(bytes) = val else { return 0 };
    let grown = bytes.len();
    let candidate = Ranked {
        rank: ctx.rank.clone(),
        value: bytes,
    };
    match slot {
        Some(Acc::RankedBytes(kept)) => {
            if candidate < *kept {
                *kept = candidate;
            }
        }
        _ => *slot = Some(Acc::RankedBytes(candidate)),
    }
    grown
}

fn min_i64(slot: &mut Option<Acc>, val: &Val) -> usize {
    if let Val::I64(value) = val {
        match slot {
            Some(Acc::I64(kept)) => *kept = (*kept).min(*value),
            _ => *slot = Some(Acc::I64(*value)),
        }
    }
    0
}

fn max_i64(slot: &mut Option<Acc>, val: &Val) -> usize {
    if let Val::I64(value) = val {
        match slot {
            Some(Acc::I64(kept)) => *kept = (*kept).max(*value),
            _ => *slot = Some(Acc::I64(*value)),
        }
    }
    0
}

fn sum_i64(slot: &mut Option<Acc>, val: &Val) -> usize {
    if let Val::I64(value) = val {
        match slot {
            Some(Acc::I64(kept)) => *kept = kept.saturating_add(*value),
            _ => *slot = Some(Acc::I64(*value)),
        }
    }
    0
}

fn sum_i32(slot: &mut Option<Acc>, val: &Val) -> usize {
    if let Val::I32(value) = val {
        match slot {
            Some(Acc::I32(kept)) => *kept = kept.saturating_add(*value),
            _ => *slot = Some(Acc::I32(*value)),
        }
    }
    0
}

/// 1e-18 USD per unit (D-A2-4).
const USD_UNITS: f64 = 1e18;

// Saturating by design: costs are far inside the range (D-A2-4).
#[allow(clippy::cast_possible_truncation)]
/// `value` in 1e-18 USD units. `as` saturates out-of-range values and maps NaN to 0.
fn usd_units(value: f64) -> i128 {
    (value * USD_UNITS).round() as i128
}

#[allow(clippy::cast_precision_loss)]
fn usd_from_units(units: i128) -> f64 {
    units as f64 / USD_UNITS
}

fn sum_usd(slot: &mut Option<Acc>, val: &Val) -> usize {
    if let Val::F64(value) = val {
        let units = usd_units(*value);
        match slot {
            Some(Acc::Usd(kept)) => *kept = kept.saturating_add(units),
            _ => *slot = Some(Acc::Usd(units)),
        }
    }
    0
}

fn str_set(slot: &mut Option<Acc>, val: Val) -> usize {
    let Val::StrList(values) = val else { return 0 };
    let Some(Acc::StrSet(set)) = slot else {
        *slot = Some(Acc::StrSet(values.iter().cloned().collect()));
        return values.iter().map(String::len).sum();
    };
    let mut grown = 0;
    for value in values {
        if set.insert(value.clone()) {
            grown += value.len();
        }
    }
    grown
}

fn str_map(slot: &mut Option<Acc>, val: Val, ctx: &RowCtx) -> usize {
    let Val::StrMap(entries) = val else { return 0 };
    if !matches!(slot, Some(Acc::StrMap(_))) {
        *slot = Some(Acc::StrMap(BTreeMap::new()));
    }
    let Some(Acc::StrMap(map)) = slot else {
        return 0;
    };
    let mut grown = 0;
    for (key, value) in entries {
        let candidate = Ranked {
            rank: ctx.rank.clone(),
            value,
        };
        grown += key.len() + candidate.value.len();
        match map.get_mut(&key) {
            Some(kept) => {
                if candidate < *kept {
                    *kept = candidate;
                }
            }
            None => {
                map.insert(key, candidate);
            }
        }
    }
    grown
}

/// One partial per trace of a [`Batch`] (`spans` schema, one project, any row order,
/// any [`Col`] encoding, any [`Selection`]). Rows: `traces` columnar schema,
/// sorted by `(start_time, trace_id)`, `partial_count = 1`.
pub(crate) fn partials_from_spans_batch(spans: &Batch) -> Result<Batch, PartialsError> {
    let mut readers: HashMap<&'static str, &Col> = HashMap::new();
    for name in SPAN_COLUMNS {
        let col = spans
            .column_by_name(name)
            .ok_or_else(|| PartialsError::column(name))?;
        if !col_matches_span_type(name, &col.data_type()) {
            return Err(PartialsError::column(name));
        }
        readers.insert(name, col);
    }

    let mut grouped: BTreeMap<[u8; 16], Vec<usize>> = BTreeMap::new();
    let trace_col = readers
        .get(spans::TRACE_ID)
        .copied()
        .ok_or_else(|| PartialsError::column(spans::TRACE_ID))?;
    for_each_active_row(spans, |row| {
        let Some(trace) = read_col_fixed16(trace_col, row) else {
            return Err(PartialsError::column(spans::TRACE_ID));
        };
        grouped.entry(trace).or_default().push(row);
        Ok(())
    })?;

    let start_col = readers
        .get(spans::START_TIME)
        .copied()
        .ok_or_else(|| PartialsError::column(spans::START_TIME))?;
    let span_col = readers
        .get(spans::SPAN_ID)
        .copied()
        .ok_or_else(|| PartialsError::column(spans::SPAN_ID))?;
    let parent_col = readers
        .get(spans::PARENT_SPAN_ID)
        .copied()
        .ok_or_else(|| PartialsError::column(spans::PARENT_SPAN_ID))?;

    let mut traces: Vec<(TraceId, TraceAcc)> = Vec::with_capacity(grouped.len());
    for (trace_bytes, rows) in grouped {
        let trace = TraceId::from_slice(&trace_bytes).map_err(|_| PartialsError::InvalidTraceId)?;
        let mut ordered: Vec<(usize, i64, [u8; 8])> = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(start) = read_col_ts(start_col, *row) else {
                return Err(PartialsError::column(spans::START_TIME));
            };
            let Some(span) = read_col_fixed8(span_col, *row) else {
                return Err(PartialsError::column(spans::SPAN_ID));
            };
            ordered.push((*row, start, span));
        }
        ordered.sort_by_key(|left| (left.1, left.2));
        let mut chosen_root: Option<[u8; 8]> = None;
        for (row, _, span) in &ordered {
            if parent_col.is_null(*row) {
                chosen_root = Some(*span);
                break;
            }
        }
        let mut acc = TraceAcc::new();
        for (row, _, _) in ordered {
            append_span_col(&mut acc, &readers, row, chosen_root)?;
        }
        *acc.slot(Column::PartialCount) = Some(Acc::I32(1));
        traces.push((trace, acc));
    }
    traces.sort_by(|(left_id, left), (right_id, right)| {
        let time = |acc: &TraceAcc| match acc.values[Column::StartTime.index()] {
            Some(Acc::I64(value)) => value,
            _ => 0,
        };
        (time(left), left_id.as_bytes()).cmp(&(time(right), right_id.as_bytes()))
    });
    build_column_batch(&traces)
}

fn col_matches_span_type(name: &str, dt: &ColType) -> bool {
    match name {
        spans::PROJECT_ID
        | spans::NAME
        | spans::SPAN_KIND
        | spans::STATUS
        | spans::SESSION_ID
        | spans::USER_ID
        | spans::ENVIRONMENT
        | spans::RELEASE
        | spans::REQUEST_MODEL
        | spans::RESPONSE_MODEL
        | spans::PROMPT_NAME
        | spans::INPUT_PREVIEW
        | spans::OUTPUT_PREVIEW => *dt == ColType::Utf8,
        spans::TRACE_ID => *dt == ColType::FixedBytes16,
        spans::SPAN_ID | spans::PARENT_SPAN_ID => *dt == ColType::FixedBytes8,
        spans::START_TIME | spans::END_TIME | spans::INGEST_TIME => {
            matches!(dt, ColType::TimestampNs { .. })
        }
        spans::TAGS => matches!(dt, ColType::List(inner) if **inner == ColType::Utf8),
        spans::METADATA => *dt == ColType::MapUtf8,
        spans::PROMPT_VERSION
        | spans::INPUT_TOKENS
        | spans::OUTPUT_TOKENS
        | spans::CACHE_READ_TOKENS
        | spans::CACHE_WRITE_TOKENS
        | spans::REASONING_TOKENS
        | spans::TOTAL_TOKENS => *dt == ColType::I64,
        spans::COST_TOTAL => *dt == ColType::F64,
        _ => false,
    }
}

fn for_each_active_row(
    batch: &Batch,
    mut f: impl FnMut(usize) -> Result<(), PartialsError>,
) -> Result<(), PartialsError> {
    let raw_len = batch.raw_len();
    match batch.selection() {
        Selection::All => {
            for row in 0..raw_len {
                f(row)?;
            }
        }
        Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                f(row)?;
            }
        }
        Selection::Indices(indices) => {
            for &row in indices {
                f(row as usize)?;
            }
        }
    }
    Ok(())
}

fn col_val(
    readers: &HashMap<&'static str, &Col>,
    name: &'static str,
    row: usize,
) -> Result<Val, PartialsError> {
    let col = readers
        .get(name)
        .copied()
        .ok_or_else(|| PartialsError::column(name))?;
    Ok(read_col_val(col, row))
}

fn append_span_col(
    acc: &mut TraceAcc,
    readers: &HashMap<&'static str, &Col>,
    row: usize,
    chosen_root: Option<[u8; 8]>,
) -> Result<(), PartialsError> {
    let Val::I64(start_time) = col_val(readers, spans::START_TIME, row)? else {
        return Err(PartialsError::column(spans::START_TIME));
    };
    let Val::Bytes(span_id) = col_val(readers, spans::SPAN_ID, row)? else {
        return Err(PartialsError::column(spans::SPAN_ID));
    };
    let root = col_val(readers, spans::PARENT_SPAN_ID, row)? == Val::Null;
    let chosen = chosen_root.is_some_and(|root_id| root_id.as_slice() == span_id.as_slice());
    let Val::Str(kind) = col_val(readers, spans::SPAN_KIND, row)? else {
        return Err(PartialsError::column(spans::SPAN_KIND));
    };
    let Val::Str(status) = col_val(readers, spans::STATUS, row)? else {
        return Err(PartialsError::column(spans::STATUS));
    };
    let Val::I64(ingest) = col_val(readers, spans::INGEST_TIME, row)? else {
        return Err(PartialsError::column(spans::INGEST_TIME));
    };
    let ctx = RowCtx {
        rank: Rank {
            root: u8::from(!chosen),
            start_time,
            tie: Some(span_id.clone()),
        },
        root,
        preview: chosen || kind == SpanKind::Llm.as_str(),
    };

    for (name, column) in DIRECT {
        acc.update(*column, col_val(readers, name, row)?, &ctx);
    }
    if ctx.preview {
        for (name, column) in [
            (spans::INPUT_PREVIEW, Column::InputPreview),
            (spans::OUTPUT_PREVIEW, Column::OutputPreview),
        ] {
            acc.update(column, col_val(readers, name, row)?, &ctx);
        }
    }
    if root {
        acc.update(Column::Status, Val::Str(status.clone()), &ctx);
        acc.update(Column::RootSpanId, Val::Bytes(span_id), &ctx);
    }
    acc.update(Column::HasRoot, Val::Bool(root), &ctx);
    let model = match col_val(readers, spans::RESPONSE_MODEL, row)? {
        Val::Str(model) => Some(model),
        _ => match col_val(readers, spans::REQUEST_MODEL, row)? {
            Val::Str(model) => Some(model),
            _ => None,
        },
    };
    if let Some(model) = model {
        acc.update(Column::Models, Val::StrList(vec![model]), &ctx);
    }
    let prompt_name = match col_val(readers, spans::PROMPT_NAME, row)? {
        Val::Str(name) => Some(name),
        _ => None,
    };
    let prompt_version = match col_val(readers, spans::PROMPT_VERSION, row)? {
        Val::I64(version) => Some(version),
        _ => None,
    };
    if let (Some(name), Some(version)) = (prompt_name, prompt_version) {
        acc.update(
            Column::Prompts,
            Val::StrList(vec![format!("{name}@{version}")]),
            &ctx,
        );
    }
    acc.update(Column::SpanCount, Val::I64(1), &ctx);
    acc.update(
        Column::ErrorCount,
        Val::I64(i64::from(status == SpanStatus::Error.as_str())),
        &ctx,
    );
    acc.update(
        Column::LlmCount,
        Val::I64(i64::from(kind == SpanKind::Llm.as_str())),
        &ctx,
    );
    acc.update(Column::LastIngestTime, Val::I64(ingest), &ctx);
    acc.update(Column::PartialCount, Val::I32(1), &ctx);
    Ok(())
}

fn resolve_flat_row(col: &Col, row: usize) -> Option<(&FlatColumn, usize)> {
    match col {
        Col::Flat(flat) => (!flat.is_null(row)).then_some((flat, row)),
        Col::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            (!dict.values().is_null(code)).then_some((dict.values(), code))
        }
        Col::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            (!rle.values().is_null(run)).then_some((rle.values(), run))
        }
        Col::Const { .. } => None,
    }
}

fn read_col_fixed16(col: &Col, row: usize) -> Option<[u8; 16]> {
    if let Col::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::FixedBytes16(b) => Some(*b),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::FixedBytes16(v) => v.get(idx).copied(),
        _ => None,
    }
}

fn read_col_fixed8(col: &Col, row: usize) -> Option<[u8; 8]> {
    if let Col::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::FixedBytes8(b) => Some(*b),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::FixedBytes8(v) => v.get(idx).copied(),
        _ => None,
    }
}

fn read_col_ts(col: &Col, row: usize) -> Option<i64> {
    if let Col::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::TimestampNs { values, .. } => values.get(idx).copied(),
        _ => None,
    }
}

fn read_col_str(col: &Col, row: usize) -> Option<&str> {
    if let Col::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Utf8(s) => Some(s.as_str()),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::Utf8(u) => u.get(idx),
        _ => None,
    }
}

fn read_col_val(col: &Col, row: usize) -> Val {
    if let Col::Const { value, len } = col {
        if row >= *len || value.is_null() {
            return Val::Null;
        }
        return match value {
            ScalarValue::Utf8(s) => Val::Str(s.clone()),
            ScalarValue::FixedBytes8(b) => Val::Bytes(b.to_vec()),
            ScalarValue::FixedBytes16(b) => Val::Bytes(b.to_vec()),
            ScalarValue::Bool(b) => Val::Bool(*b),
            ScalarValue::I64(v) => Val::I64(*v),
            ScalarValue::F64(v) => Val::F64(*v),
            ScalarValue::TimestampNs { value, .. } => Val::I64(*value),
            ScalarValue::List { values, .. } => Val::StrList(
                values
                    .iter()
                    .filter_map(|v| match v {
                        ScalarValue::Utf8(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            ScalarValue::MapUtf8(entries) => Val::StrMap(
                entries
                    .iter()
                    .filter_map(|(k, v)| v.as_ref().map(|val| (k.clone(), val.clone())))
                    .collect(),
            ),
            _ => Val::Null,
        };
    }
    let Some((flat, idx)) = resolve_flat_row(col, row) else {
        return Val::Null;
    };
    match flat.data() {
        FlatData::Utf8(u) => u.get(idx).map_or(Val::Null, |s| Val::Str(s.to_owned())),
        FlatData::FixedBytes8(v) => v.get(idx).map_or(Val::Null, |b| Val::Bytes(b.to_vec())),
        FlatData::FixedBytes16(v) => v.get(idx).map_or(Val::Null, |b| Val::Bytes(b.to_vec())),
        FlatData::Bool(bm) => {
            if idx < bm.len() {
                Val::Bool(bm.get(idx))
            } else {
                Val::Null
            }
        }
        FlatData::I64(v) => v.get(idx).copied().map_or(Val::Null, Val::I64),
        FlatData::F64(v) => v.get(idx).copied().map_or(Val::Null, Val::F64),
        FlatData::TimestampNs { values, .. } => {
            values.get(idx).copied().map_or(Val::Null, Val::I64)
        }
        FlatData::List {
            offsets, values, ..
        } => {
            let (Some(&start), Some(&end)) = (offsets.get(idx), offsets.get(idx + 1)) else {
                return Val::Null;
            };
            let mut out = Vec::with_capacity((end as usize).saturating_sub(start as usize));
            for i in (start as usize)..(end as usize) {
                if let Some(s) = read_col_str(values, i) {
                    out.push(s.to_owned());
                }
            }
            Val::StrList(out)
        }
        FlatData::MapUtf8(map) => {
            let Some(entries) = map.get(idx) else {
                return Val::Null;
            };
            Val::StrMap(
                entries
                    .into_iter()
                    .filter_map(|(k, v)| v.map(|val| (k, val)))
                    .collect(),
            )
        }
        _ => Val::Null,
    }
}

fn build_column_batch(traces: &[(TraceId, TraceAcc)]) -> Result<Batch, PartialsError> {
    let schema = crate::catalog::FileTable::Traces.column_schema();
    let n = traces.len();
    if n == 0 {
        return Ok(Batch::empty(schema));
    }
    let mut columns = Vec::with_capacity(Column::ALL.len());
    for column in Column::ALL {
        columns.push(build_col_column(column, traces)?);
    }
    Batch::try_new_all(schema, columns, n).map_err(PartialsError::from_column)
}

fn opt_nulls(nulls: Bitmap) -> Option<Bitmap> {
    if nulls.all_ones() { None } else { Some(nulls) }
}

fn build_col_column(column: Column, traces: &[(TraceId, TraceAcc)]) -> Result<Col, PartialsError> {
    let n = traces.len();
    let slot_idx = column.index();
    let flat = match column {
        Column::Status => {
            let vals: Vec<String> = traces.iter().map(|(_, acc)| status_of(acc)).collect();
            let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
            let data = Utf8Data::try_from_slices(refs).map_err(PartialsError::from_column)?;
            FlatColumn::try_new(FlatData::Utf8(data), None).map_err(PartialsError::from_column)?
        }
        Column::ProjectId
        | Column::Name
        | Column::SessionId
        | Column::UserId
        | Column::Environment
        | Column::Release
        | Column::InputPreview
        | Column::OutputPreview => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, (_, acc)) in traces.iter().enumerate() {
                match &acc.values[slot_idx] {
                    Some(Acc::RankedStr(ranked)) => vals.push(ranked.value.as_str()),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push("");
                    }
                }
            }
            let data = Utf8Data::try_from_slices(vals).map_err(PartialsError::from_column)?;
            FlatColumn::try_new(FlatData::Utf8(data), opt_nulls(nulls))
                .map_err(PartialsError::from_column)?
        }
        Column::TraceId => {
            let mut vals = Vec::with_capacity(n);
            for (trace_id, acc) in traces {
                if let Some(Acc::Bytes(bytes)) = &acc.values[slot_idx]
                    && let Ok(arr) = <[u8; 16]>::try_from(bytes.as_slice())
                {
                    vals.push(arr);
                } else {
                    vals.push(*trace_id.as_bytes());
                }
            }
            FlatColumn::try_new(FlatData::FixedBytes16(vals), None)
                .map_err(PartialsError::from_column)?
        }
        Column::RootSpanId => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, (_, acc)) in traces.iter().enumerate() {
                if let Some(Acc::RankedBytes(ranked)) = &acc.values[slot_idx]
                    && let Ok(arr) = <[u8; 8]>::try_from(ranked.value.as_slice())
                {
                    vals.push(arr);
                } else {
                    let _ = nulls.set(i, false);
                    vals.push([0u8; 8]);
                }
            }
            FlatColumn::try_new(FlatData::FixedBytes8(vals), opt_nulls(nulls))
                .map_err(PartialsError::from_column)?
        }
        Column::HasRoot => {
            let mut bm = Bitmap::new_zeroed(n);
            for (i, (_, acc)) in traces.iter().enumerate() {
                if let Some(Acc::Bool(true)) = &acc.values[slot_idx] {
                    let _ = bm.set(i, true);
                }
            }
            FlatColumn::try_new(FlatData::Bool(bm), None).map_err(PartialsError::from_column)?
        }
        Column::StartTime | Column::EndTime | Column::LastIngestTime => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, (_, acc)) in traces.iter().enumerate() {
                match &acc.values[slot_idx] {
                    Some(Acc::I64(value)) => vals.push(*value),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push(0);
                    }
                }
            }
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vals,
                    utc: true,
                },
                opt_nulls(nulls),
            )
            .map_err(PartialsError::from_column)?
        }
        Column::SpanCount
        | Column::ErrorCount
        | Column::LlmCount
        | Column::InputTokens
        | Column::OutputTokens
        | Column::CacheReadTokens
        | Column::CacheWriteTokens
        | Column::ReasoningTokens
        | Column::TotalTokens => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, (_, acc)) in traces.iter().enumerate() {
                match &acc.values[slot_idx] {
                    Some(Acc::I64(value)) => vals.push(*value),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push(0);
                    }
                }
            }
            FlatColumn::try_new(FlatData::I64(vals), opt_nulls(nulls))
                .map_err(PartialsError::from_column)?
        }
        Column::PartialCount => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, (_, acc)) in traces.iter().enumerate() {
                match &acc.values[slot_idx] {
                    Some(Acc::I32(value)) => vals.push(i64::from(*value)),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push(0);
                    }
                }
            }
            FlatColumn::try_new(FlatData::I64(vals), opt_nulls(nulls))
                .map_err(PartialsError::from_column)?
        }
        Column::CostTotal => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, (_, acc)) in traces.iter().enumerate() {
                match &acc.values[slot_idx] {
                    Some(Acc::Usd(units)) => vals.push(usd_from_units(*units)),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push(0.0);
                    }
                }
            }
            FlatColumn::try_new(FlatData::F64(vals), opt_nulls(nulls))
                .map_err(PartialsError::from_column)?
        }
        Column::Tags | Column::Models | Column::Prompts => {
            let mut offsets = Vec::with_capacity(n + 1);
            offsets.push(0u32);
            let mut items: Vec<&str> = Vec::new();
            for (_, acc) in traces {
                if let Some(Acc::StrSet(set)) = &acc.values[slot_idx] {
                    for s in set {
                        items.push(s.as_str());
                    }
                }
                let end = u32::try_from(items.len()).map_err(|_| {
                    PartialsError::from_column(cetra_column::ColumnError::Overflow {
                        reason: "list items exceed u32::MAX",
                    })
                })?;
                offsets.push(end);
            }
            let child = FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(items).map_err(PartialsError::from_column)?,
                ),
                None,
            )
            .map_err(PartialsError::from_column)?;
            FlatColumn::try_new(
                FlatData::List {
                    item_type: ColType::Utf8,
                    offsets,
                    values: Box::new(Col::Flat(child)),
                },
                None,
            )
            .map_err(PartialsError::from_column)?
        }
        Column::Metadata => {
            let mut offsets = Vec::with_capacity(n + 1);
            offsets.push(0u32);
            let mut keys: Vec<&str> = Vec::new();
            let mut vals: Vec<&str> = Vec::new();
            for (_, acc) in traces {
                if let Some(Acc::StrMap(map)) = &acc.values[slot_idx] {
                    for (k, r) in map {
                        keys.push(k.as_str());
                        vals.push(r.value.as_str());
                    }
                }
                let end = u32::try_from(keys.len()).map_err(|_| {
                    PartialsError::from_column(cetra_column::ColumnError::Overflow {
                        reason: "map entries exceed u32::MAX",
                    })
                })?;
                offsets.push(end);
            }
            let keys_data = Utf8Data::try_from_slices(keys).map_err(PartialsError::from_column)?;
            let vals_data = Utf8Data::try_from_slices(vals).map_err(PartialsError::from_column)?;
            let map_data = MapData::try_new(offsets, keys_data, vals_data, None)
                .map_err(PartialsError::from_column)?;
            FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), None)
                .map_err(PartialsError::from_column)?
        }
    };
    Ok(Col::Flat(flat))
}

/// Spans columns the span-level rules read. Some map straight to a `traces`
/// column; the rest are handled in [`append_span`].
const SPAN_COLUMNS: &[&str] = &[
    spans::PROJECT_ID,
    spans::TRACE_ID,
    spans::SPAN_ID,
    spans::PARENT_SPAN_ID,
    spans::NAME,
    spans::SPAN_KIND,
    spans::START_TIME,
    spans::END_TIME,
    spans::STATUS,
    spans::SESSION_ID,
    spans::USER_ID,
    spans::ENVIRONMENT,
    spans::RELEASE,
    spans::TAGS,
    spans::METADATA,
    spans::REQUEST_MODEL,
    spans::RESPONSE_MODEL,
    spans::PROMPT_NAME,
    spans::PROMPT_VERSION,
    spans::INPUT_PREVIEW,
    spans::OUTPUT_PREVIEW,
    spans::INPUT_TOKENS,
    spans::OUTPUT_TOKENS,
    spans::CACHE_READ_TOKENS,
    spans::CACHE_WRITE_TOKENS,
    spans::REASONING_TOKENS,
    spans::TOTAL_TOKENS,
    spans::COST_TOTAL,
    spans::INGEST_TIME,
];

/// Spans columns whose name is also a `traces` column name.
const DIRECT: &[(&str, Column)] = &[
    (spans::PROJECT_ID, Column::ProjectId),
    (spans::TRACE_ID, Column::TraceId),
    (spans::NAME, Column::Name),
    (spans::START_TIME, Column::StartTime),
    (spans::END_TIME, Column::EndTime),
    (spans::SESSION_ID, Column::SessionId),
    (spans::USER_ID, Column::UserId),
    (spans::ENVIRONMENT, Column::Environment),
    (spans::RELEASE, Column::Release),
    (spans::TAGS, Column::Tags),
    (spans::METADATA, Column::Metadata),
    (spans::INPUT_TOKENS, Column::InputTokens),
    (spans::OUTPUT_TOKENS, Column::OutputTokens),
    (spans::CACHE_READ_TOKENS, Column::CacheReadTokens),
    (spans::CACHE_WRITE_TOKENS, Column::CacheWriteTokens),
    (spans::REASONING_TOKENS, Column::ReasoningTokens),
    (spans::TOTAL_TOKENS, Column::TotalTokens),
    (spans::COST_TOTAL, Column::CostTotal),
];

fn status_of(acc: &TraceAcc) -> String {
    let errors = match acc.values[Column::ErrorCount.index()] {
        Some(Acc::I64(value)) => value,
        _ => 0,
    };
    if errors > 0 {
        return SpanStatus::Error.as_str().to_owned();
    }
    match &acc.values[Column::Status.index()] {
        Some(Acc::RankedStr(ranked)) => ranked.value.clone(),
        _ => SpanStatus::Unset.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::mem::size_of;
    use std::sync::Arc;

    use cetra_column::{
        Batch, Bitmap, Column as Col, DictColumn, FlatColumn, FlatData, RleColumn, ScalarValue,
        Selection, Utf8Data,
    };
    use cetra_core::batch::spans_to_batch;
    use cetra_core::enums::{SpanKind, SpanStatus};
    use cetra_core::ids::{ProjectId, SpanId, TraceId};
    use cetra_core::schema::{spans, traces, traces_schema};
    use cetra_core::span::Span;
    use proptest::prelude::*;

    use super::{Column, MergeColumns, partials_from_spans_batch};
    use crate::engine::traces_source::SortedTraceMerger;
    use crate::error::PartialsError;
    use crate::test_support::{dataset_spans, trace_batch};

    const KINDS: [SpanKind; 4] = [
        SpanKind::Llm,
        SpanKind::Tool,
        SpanKind::Chain,
        SpanKind::Span,
    ];
    const STATUSES: [SpanStatus; 3] = [SpanStatus::Unset, SpanStatus::Ok, SpanStatus::Error];
    const WORDS: &[&str] = &["a", "b", "c", "d"];

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn trace_id(last: u8) -> TraceId {
        let mut bytes = [0_u8; 16];
        bytes[15] = last;
        TraceId::from_bytes(bytes).unwrap()
    }

    fn span_id(last: u8) -> SpanId {
        let mut bytes = [0_u8; 8];
        bytes[7] = last;
        SpanId::from_bytes(bytes).unwrap()
    }

    /// A span with only the required fields set; tests override what they need.
    fn base_span(trace: TraceId, span: SpanId, start: i64) -> Span {
        Span {
            project_id: project(),
            trace_id: trace,
            span_id: span,
            parent_span_id: None,
            name: format!("span-{start}"),
            span_kind: SpanKind::Span,
            start_time: start,
            end_time: start + 10,
            status: SpanStatus::Unset,
            status_message: None,
            service_name: None,
            session_id: None,
            user_id: None,
            environment: None,
            release: None,
            tags: Vec::new(),
            metadata: BTreeMap::new(),
            provider: None,
            request_model: None,
            response_model: None,
            prompt_name: None,
            prompt_version: None,
            operation: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: None,
            cost_input: None,
            cost_output: None,
            cost_total: None,
            cost_source: None,
            ttft_ns: None,
            request_params: None,
            input: None,
            input_format: None,
            output: None,
            output_format: None,
            input_preview: None,
            output_preview: None,
            system_instructions: None,
            tool_definitions: None,
            attributes: BTreeMap::new(),
            resource_attributes: BTreeMap::new(),
            scope_name: None,
            scope_version: None,
            events: Vec::new(),
            links: Vec::new(),
            media_refs: Vec::new(),
            truncated: Vec::new(),
            conventions: Vec::new(),
            ingest_time: start + 1,
            schema_version: 1,
        }
    }

    fn spans_batch(spans: &[Span]) -> Batch {
        spans_to_batch(spans).unwrap()
    }

    /// Sorts a batch by `trace_id` so two batches can be compared row for row.
    fn sort_by_trace_id(batch: &Batch) -> Batch {
        let trace_col = batch.column_by_name(traces::TRACE_ID).unwrap();
        let mut order: Vec<(ScalarValue, u32)> = (0..batch.num_rows())
            .map(|row| (trace_col.get_scalar(row).unwrap(), row as u32))
            .collect();
        order.sort_by(|(a, _), (b, _)| match (a, b) {
            (ScalarValue::FixedBytes16(x), ScalarValue::FixedBytes16(y)) => x.cmp(y),
            _ => panic!("trace_id must be FixedBytes16"),
        });
        let indices: Vec<u32> = order.into_iter().map(|(_, row)| row).collect();
        let cols: Vec<Col> = batch
            .columns()
            .iter()
            .map(|col| Col::Flat(col.take(&indices, None).unwrap()))
            .collect();
        Batch::try_new_all(Arc::clone(batch.schema()), cols, indices.len()).unwrap()
    }

    /// Compares every column exactly, except `cost_total` within a relative 1e-9.
    fn assert_batches_equal(actual: &Batch, expected: &Batch) {
        assert_eq!(
            actual.schema().fields(),
            expected.schema().fields(),
            "schema"
        );
        assert_eq!(actual.num_rows(), expected.num_rows(), "row count");
        for field in actual.schema().fields() {
            let name = field.name.as_str();
            let left = actual.column_by_name(name).unwrap();
            let right = expected.column_by_name(name).unwrap();
            for row in 0..actual.num_rows() {
                let lv = left.get_scalar(row).unwrap();
                let rv = right.get_scalar(row).unwrap();
                if name == traces::COST_TOTAL {
                    match (&lv, &rv) {
                        (ScalarValue::Null(_), ScalarValue::Null(_)) => {}
                        (ScalarValue::F64(a), ScalarValue::F64(b)) => {
                            let tolerance = 1e-9 * a.abs().max(b.abs()).max(1.0);
                            assert!((a - b).abs() <= tolerance, "cost {a} vs {b}");
                        }
                        _ => panic!("cost mismatch at row {row}: {lv:?} vs {rv:?}"),
                    }
                } else {
                    assert_eq!(lv, rv, "column {name} row {row}");
                }
            }
        }
    }

    /// Selects columns by name, in the given order.
    fn project_columns(batch: &Batch, names: &[&str]) -> Batch {
        let indices: Vec<usize> = names
            .iter()
            .map(|name| batch.schema().index_of(name).unwrap())
            .collect();
        batch.project(&indices).unwrap()
    }

    /// Replaces `partial_count` with the number of groups each trace was cut into.
    fn with_partial_counts(batch: &Batch, counts: &BTreeMap<Vec<u8>, i32>) -> Batch {
        let ids = batch.column_by_name(traces::TRACE_ID).unwrap();
        let values: Vec<i64> = (0..batch.num_rows())
            .map(|row| match ids.get_scalar(row).unwrap() {
                ScalarValue::FixedBytes16(bytes) => {
                    i64::from(*counts.get(bytes.as_slice()).unwrap_or(&1))
                }
                other => panic!("unexpected trace_id {other:?}"),
            })
            .collect();
        let mut columns = batch.columns().to_vec();
        let index = batch.schema().index_of(traces::PARTIAL_COUNT).unwrap();
        columns[index] = Arc::new(Col::Flat(
            FlatColumn::try_new(FlatData::I64(values), None).unwrap(),
        ));
        Batch::try_new_shared(
            Arc::clone(batch.schema()),
            columns,
            batch.num_rows(),
            Selection::All,
        )
        .unwrap()
    }

    /// Deterministic draws from a proptest-generated byte stream.
    struct Draw<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    impl<'a> Draw<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, at: 0 }
        }

        fn byte(&mut self) -> u8 {
            let value = self.bytes[self.at % self.bytes.len()];
            self.at += 1;
            value
        }

        fn pick(&mut self, n: usize) -> usize {
            usize::from(self.byte()) % n
        }

        fn flag(&mut self) -> bool {
            self.byte().is_multiple_of(2)
        }

        fn word(&mut self) -> String {
            WORDS[self.pick(WORDS.len())].to_owned()
        }

        fn opt_word(&mut self) -> Option<String> {
            self.flag().then(|| self.word())
        }

        fn opt_i64(&mut self) -> Option<i64> {
            self.flag().then(|| i64::from(self.byte()))
        }

        fn opt_f64(&mut self) -> Option<f64> {
            self.flag().then(|| f64::from(self.byte()) / 8.0)
        }
    }

    struct Generated {
        all: Vec<Span>,
        groups: Vec<Vec<Span>>,
        counts: BTreeMap<Vec<u8>, i32>,
    }

    /// Builds traces and cuts each into contiguous groups. `restricted` keeps
    /// inside "What a merge can't rebuild": distinct start times, at most one
    /// root, uniform context fields and metadata, previews only on the root.
    /// Unrestricted allows ties, several roots, differing metadata and context
    /// values, and previews on any span.
    fn generate(bytes: &[u8], restricted: bool) -> Generated {
        let mut draw = Draw::new(bytes);
        let trace_count = 1 + draw.pick(3);
        let mut all = Vec::new();
        let mut groups = Vec::new();
        let mut counts = BTreeMap::new();
        let mut next_span = 1_u8;
        for trace_index in 0..trace_count {
            let trace = trace_id(trace_index as u8 + 1);
            let count = 1 + draw.pick(12);
            let root = if restricted {
                if draw.flag() {
                    Some(draw.pick(count))
                } else {
                    None
                }
            } else {
                // Unrestricted: any span may be a root, so several roots happen.
                None
            };
            let session = draw.opt_word();
            let user = draw.opt_word();
            let environment = draw.opt_word();
            let release = draw.opt_word();
            let mut metadata = BTreeMap::new();
            for key in ["k1", "k2", "k3"] {
                if draw.flag() {
                    metadata.insert(key.to_owned(), format!("v-{key}"));
                }
            }
            let preview = draw.opt_word();
            let mut trace_spans = Vec::with_capacity(count);
            for index in 0..count {
                let start = if restricted {
                    1_000 + i64::try_from(index).unwrap() * 10
                } else {
                    1_000 + i64::try_from(draw.pick(4)).unwrap() * 10
                };
                let id = span_id(next_span);
                next_span = next_span.wrapping_add(1).max(1);
                let mut span = base_span(trace, id, start);
                let is_root = if restricted {
                    root == Some(index)
                } else {
                    draw.flag()
                };
                span.parent_span_id = (!is_root).then(|| span_id(9));
                span.span_kind = KINDS[draw.pick(KINDS.len())];
                span.status = STATUSES[draw.pick(STATUSES.len())];
                span.name = draw.word();
                if restricted {
                    span.session_id = session.clone();
                    span.user_id = user.clone();
                    span.environment = environment.clone();
                    span.release = release.clone();
                    span.metadata = metadata.clone();
                    span.input_preview = is_root.then(|| preview.clone()).flatten();
                    span.output_preview = is_root.then(|| preview.clone()).flatten();
                } else {
                    span.session_id = draw.opt_word();
                    span.user_id = draw.opt_word();
                    span.environment = draw.opt_word();
                    span.release = draw.opt_word();
                    for key in ["k1", "k2", "k3"] {
                        if draw.flag() {
                            span.metadata.insert(key.to_owned(), draw.word());
                        }
                    }
                    span.input_preview = draw.opt_word();
                    span.output_preview = draw.opt_word();
                }
                span.tags = (0..draw.pick(3)).map(|_| draw.word()).collect();
                if draw.flag() {
                    span.response_model = Some(draw.word());
                } else if draw.flag() {
                    span.request_model = Some(draw.word());
                }
                span.input_tokens = draw.opt_i64();
                span.output_tokens = draw.opt_i64();
                span.cache_read_tokens = draw.opt_i64();
                span.cache_write_tokens = draw.opt_i64();
                span.reasoning_tokens = draw.opt_i64();
                span.total_tokens = draw.opt_i64();
                span.cost_total = draw.opt_f64();
                span.ingest_time = start + 1 + i64::try_from(draw.pick(4)).unwrap();
                span.end_time = start + 10 + i64::try_from(draw.pick(4)).unwrap();
                trace_spans.push(span);
            }
            trace_spans.sort_by_key(|span| (span.start_time, span.span_id));
            let group_count = 1 + draw.pick(4);
            let group_count = group_count.min(count);
            counts.insert(
                trace.as_bytes().to_vec(),
                i32::try_from(group_count).unwrap(),
            );
            // Contiguous cuts, as flushes produce.
            let mut remaining = trace_spans.len();
            let mut at = 0;
            for group in 0..group_count {
                let left = group_count - group;
                let take = if group + 1 == group_count {
                    remaining
                } else {
                    1 + draw.pick(remaining - left + 1)
                };
                groups.push(trace_spans[at..at + take].to_vec());
                at += take;
                remaining -= take;
            }
            all.extend(trace_spans);
        }
        Generated {
            all,
            groups,
            counts,
        }
    }

    /// Shuffles a list of partial batches deterministically.
    fn shuffled(mut batches: Vec<Batch>, bytes: &[u8]) -> Vec<Batch> {
        let mut draw = Draw::new(bytes);
        for index in (1..batches.len()).rev() {
            batches.swap(index, draw.pick(index + 1));
        }
        batches
    }

    fn partial_batches(groups: &[Vec<Span>]) -> Vec<Batch> {
        groups
            .iter()
            .map(|group| partials_from_spans_batch(&spans_batch(group)).unwrap())
            .collect()
    }

    fn merge_all(batches: &[Batch]) -> Batch {
        let schema = traces_schema();
        let mut merger = SortedTraceMerger::new(&MergeColumns::all(), &schema).unwrap();
        if !batches.is_empty() {
            let refs: Vec<&Batch> = batches.iter().collect();
            let combined = Batch::concat(Arc::clone(&schema), &refs, None).unwrap();
            let sorted = sort_by_trace_id(&combined);
            merger.push_range(&sorted, 0, sorted.raw_len()).unwrap();
        }
        merger.finish().unwrap()
    }

    #[test]
    fn partials_from_spans_matches_the_a2_oracle() {
        let spans = dataset_spans();
        let batch = spans_batch(&spans);
        let partials = partials_from_spans_batch(&batch).unwrap();
        let oracle = trace_batch(&spans);
        assert_batches_equal(&sort_by_trace_id(&partials), &sort_by_trace_id(&oracle));
    }

    /// Spec 01 § Order: the choice among several root spans is the earliest
    /// one. Only that span ranks ahead of the others, so a later root must not
    /// outrank an earlier non-root span for context fields, metadata or
    /// previews. Before the fix the later root won all three.
    #[test]
    fn several_root_spans_use_only_the_earliest_as_root() {
        let trace = trace_id(1);
        let mut root_1 = base_span(trace, span_id(1), 10);
        root_1.name = "root-1".to_owned();
        let mut child = base_span(trace, span_id(2), 20);
        child.parent_span_id = Some(span_id(1));
        child.span_kind = SpanKind::Llm;
        child.session_id = Some("from-child".to_owned());
        child
            .metadata
            .insert("k".to_owned(), "from-child".to_owned());
        child.input_preview = Some("child-preview".to_owned());
        let mut root_3 = base_span(trace, span_id(3), 30);
        root_3.name = "root-3".to_owned();
        root_3.session_id = Some("from-root-3".to_owned());
        root_3
            .metadata
            .insert("k".to_owned(), "from-root-3".to_owned());
        root_3.input_preview = Some("root-3-preview".to_owned());

        let spans = [root_1, child, root_3];
        let partials = partials_from_spans_batch(&spans_batch(&spans)).unwrap();
        let oracle = trace_batch(&spans);
        assert_batches_equal(&sort_by_trace_id(&partials), &sort_by_trace_id(&oracle));
    }

    /// Every accumulator slot must be accounted for in `size()`.
    #[test]
    fn size_accounts_for_every_accumulator_slot() {
        let schema = traces_schema();
        let mut merger = SortedTraceMerger::new(&MergeColumns::all(), &schema).unwrap();
        let partials = partials_from_spans_batch(&spans_batch(&[
            base_span(trace_id(1), span_id(1), 10),
            base_span(trace_id(2), span_id(2), 20),
        ]))
        .unwrap();
        let sorted = sort_by_trace_id(&partials);
        merger.push_range(&sorted, 0, sorted.raw_len()).unwrap();
        assert!(merger.size() > 2 * size_of::<usize>() * Column::ALL.len());
    }

    #[test]
    fn merge_columns_are_closed_under_the_rules() {
        let columns = MergeColumns::new(&[traces::STATUS, traces::TAGS]);
        assert_eq!(
            columns.output(),
            vec![
                traces::TRACE_ID,
                traces::START_TIME,
                traces::STATUS,
                traces::TAGS
            ]
        );
        let input = columns.input();
        for key in [
            traces::TRACE_ID,
            traces::START_TIME,
            traces::HAS_ROOT,
            traces::ROOT_SPAN_ID,
            traces::ERROR_COUNT,
            traces::STATUS,
            traces::TAGS,
        ] {
            assert!(input.contains(&key), "missing {key}");
        }
        assert_eq!(
            MergeColumns::new(&[]).output(),
            vec![traces::TRACE_ID, traces::START_TIME]
        );
        // output() and input() are in traces_schema() order.
        let order: Vec<usize> = columns
            .output()
            .iter()
            .map(|name| traces_schema().index_of(name).unwrap())
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn no_columns_requested_outputs_only_the_keys() {
        let columns = MergeColumns::new(&[]);
        let partials = partials_from_spans_batch(&spans_batch(&[
            base_span(trace_id(1), span_id(1), 10),
            base_span(trace_id(2), span_id(2), 20),
        ]))
        .unwrap();
        let projected = sort_by_trace_id(&project_columns(&partials, &columns.input()));
        let schema = traces_schema();
        let mut merger = SortedTraceMerger::new(&columns, &schema).unwrap();
        assert_eq!(merger.len(), 0);
        merger
            .push_range(&projected, 0, projected.raw_len())
            .unwrap();
        assert_eq!(merger.len(), 2);
        assert!(merger.size() > 0);
        let result = merger.finish().unwrap();
        assert_eq!(result.columns().len(), 2);
        assert_eq!(result.num_rows(), 2);
        assert_eq!(
            result
                .schema()
                .fields()
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec![traces::TRACE_ID, traces::START_TIME]
        );
    }

    #[test]
    fn a_merge_of_zero_batches_gives_zero_rows() {
        let schema = traces_schema();
        let result = SortedTraceMerger::new(&MergeColumns::all(), &schema)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(result.num_rows(), 0);
        assert_eq!(result.columns().len(), schema.fields().len());
    }

    #[test]
    fn one_span_gives_span_count_one() {
        let span = base_span(trace_id(1), span_id(1), 10);
        let partial = partials_from_spans_batch(&spans_batch(&[span])).unwrap();
        assert_eq!(
            partial
                .column_by_name(traces::SPAN_COUNT)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::I64(1)
        );
    }

    /// D-V-15: `prompts` is the distinct union of `name@version` over the spans of
    /// a trace, across partials. A name-only span adds nothing.
    #[test]
    fn prompts_are_a_distinct_union() {
        let trace = trace_id(1);
        let mut first = base_span(trace, span_id(1), 10);
        first.prompt_name = Some("a".to_owned());
        first.prompt_version = Some(1);
        let mut second = base_span(trace, span_id(2), 20);
        second.parent_span_id = Some(span_id(1));
        second.prompt_name = Some("a".to_owned());
        second.prompt_version = Some(1);
        let mut third = base_span(trace, span_id(3), 30);
        third.parent_span_id = Some(span_id(1));
        third.prompt_name = Some("b".to_owned());
        third.prompt_version = Some(2);
        let mut name_only = base_span(trace, span_id(4), 40);
        name_only.parent_span_id = Some(span_id(1));
        name_only.prompt_name = Some("c".to_owned());
        let partials = partial_batches(&[vec![first, second], vec![third, name_only]]);
        let merged = merge_all(&partials);
        assert_eq!(
            merged
                .column_by_name(traces::PROMPTS)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::List {
                values: vec![
                    ScalarValue::Utf8("a@1".to_owned()),
                    ScalarValue::Utf8("b@2".to_owned()),
                ],
                item_type: cetra_column::DataType::Utf8,
            }
        );
    }

    #[test]
    fn all_costs_null_give_a_null_sum() {
        let one = base_span(trace_id(1), span_id(1), 10);
        let mut two = base_span(trace_id(1), span_id(2), 20);
        two.parent_span_id = Some(span_id(9));
        let partials = partial_batches(&[vec![one], vec![two]]);
        let merged = merge_all(&partials);
        assert!(matches!(
            merged
                .column_by_name(traces::COST_TOTAL)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::Null(_)
        ));
    }

    /// D-A2-4: the merged `cost_total` is the same bit for bit whatever order
    /// the partials arrive in.
    #[test]
    fn cost_merge_does_not_depend_on_order() {
        let mut one = base_span(trace_id(1), span_id(1), 10);
        one.cost_total = Some(0.1);
        let mut two = base_span(trace_id(1), span_id(2), 20);
        two.parent_span_id = Some(span_id(9));
        two.cost_total = Some(0.2);
        let mut three = base_span(trace_id(1), span_id(3), 30);
        three.parent_span_id = Some(span_id(9));
        three.cost_total = Some(0.3);
        let partials = partial_batches(&[vec![one], vec![two], vec![three]]);
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let mut values = Vec::new();
        for order in orders {
            let ordered: Vec<Batch> = order.iter().map(|index| partials[*index].clone()).collect();
            let merged = merge_all(&ordered);
            let ScalarValue::F64(value) = merged
                .column_by_name(traces::COST_TOTAL)
                .unwrap()
                .get_scalar(0)
                .unwrap()
            else {
                panic!("expected f64 cost_total");
            };
            assert!((value - 0.6).abs() < 1e-15, "cost {value} for {order:?}");
            values.push(value);
        }
        let bits: Vec<u64> = values.iter().map(|value| value.to_bits()).collect();
        assert!(
            bits.iter().all(|value| *value == bits[0]),
            "cost depends on order: {values:?}"
        );
    }

    #[test]
    fn usd_units_round_trip() {
        use super::{usd_from_units, usd_units};
        assert_eq!(usd_from_units(usd_units(0.1)), 0.1);
        assert_eq!(usd_units(0.0), 0);
        assert_eq!(usd_units(-0.25), -250_000_000_000_000_000);
        assert_eq!(usd_units(f64::NAN), 0);
        assert_eq!(usd_units(f64::INFINITY), i128::MAX);
    }

    #[test]
    fn a_root_partial_with_null_session_takes_the_earliest_non_root() {
        let mut root = base_span(trace_id(1), span_id(1), 100);
        root.session_id = None;
        let mut earlier = base_span(trace_id(1), span_id(2), 10);
        earlier.parent_span_id = Some(span_id(9));
        earlier.session_id = Some("earlier".to_owned());
        let mut later = base_span(trace_id(1), span_id(3), 50);
        later.parent_span_id = Some(span_id(9));
        later.session_id = Some("later".to_owned());
        let merged = merge_all(&partial_batches(&[vec![root], vec![earlier], vec![later]]));
        assert_eq!(
            merged
                .column_by_name(traces::SESSION_ID)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::Utf8("earlier".to_owned())
        );
    }

    #[test]
    fn malformed_partial_batches_return_an_error() {
        let partials = partials_from_spans_batch(&spans_batch(&[
            base_span(trace_id(1), span_id(1), 10),
            base_span(trace_id(1), span_id(2), 20),
        ]))
        .unwrap();
        let all = MergeColumns::all();
        let schema = traces_schema();

        // Missing `has_root`.
        let missing: Vec<&str> = all
            .input()
            .iter()
            .copied()
            .filter(|name| *name != traces::HAS_ROOT)
            .collect();
        let projected = project_columns(&partials, &missing);
        let mut merger = SortedTraceMerger::new(&MergeColumns::all(), &schema).unwrap();
        assert!(
            merger
                .push_range(&projected, 0, projected.raw_len())
                .is_err()
        );

        // `has_root` with the wrong column type.
        let names = all.input();
        let mut wrong_fields = Vec::with_capacity(names.len());
        let mut wrong_cols = Vec::with_capacity(names.len());
        for name in names {
            if name == traces::HAS_ROOT {
                wrong_fields.push(cetra_column::Field::new(
                    name,
                    cetra_column::DataType::I64,
                    true,
                ));
                wrong_cols.push(Arc::new(Col::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![1_i64; partials.num_rows()]), None)
                        .unwrap(),
                )));
            } else {
                let idx = partials.schema().index_of(name).unwrap();
                wrong_fields.push(partials.schema().fields()[idx].clone());
                wrong_cols.push(Arc::clone(&partials.columns()[idx]));
            }
        }
        let wrong = Batch::try_new_shared(
            cetra_column::Schema::try_new_arc(wrong_fields).unwrap(),
            wrong_cols,
            partials.num_rows(),
            Selection::All,
        )
        .unwrap();
        let mut merger = SortedTraceMerger::new(&MergeColumns::all(), &schema).unwrap();
        assert!(merger.push_range(&wrong, 0, wrong.raw_len()).is_err());
    }

    #[test]
    fn a_malformed_spans_batch_returns_a_column_error() {
        let good = spans_batch(&[base_span(trace_id(1), span_id(1), 10)]);
        let idx = good.schema().index_of(spans::START_TIME).unwrap();
        let mut fields = good.schema().fields().to_vec();
        let mut columns = good.columns().to_vec();
        fields[idx] =
            cetra_column::Field::new(spans::START_TIME, cetra_column::DataType::I64, false);
        columns[idx] = Arc::new(Col::Flat(
            FlatColumn::try_new(FlatData::I64(vec![10_i64]), None).unwrap(),
        ));
        let wrong = Batch::try_new_shared(
            cetra_column::Schema::try_new_arc(fields).unwrap(),
            columns,
            1,
            Selection::All,
        )
        .unwrap();
        let error = partials_from_spans_batch(&wrong).unwrap_err();
        assert!(matches!(error, PartialsError::Column { .. }));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Excluded by construction: no ties on the earliest start_time across
        /// partials, several roots, previews on non-root spans, metadata or
        /// context values that differ between spans (§ What a merge can't rebuild).
        #[test]
        fn split_and_merge_rebuilds_the_partial(bytes in prop::collection::vec(any::<u8>(), 64..512)) {
            let generated = generate(&bytes, true);
            let expected = with_partial_counts(
                &sort_by_trace_id(&partials_from_spans_batch(&spans_batch(&generated.all)).unwrap()),
                &generated.counts,
            );
            let batches = shuffled(partial_batches(&generated.groups), &bytes);
            let merged = merge_all(&batches);
            assert_batches_equal(&sort_by_trace_id(&merged), &expected);
        }

        /// Unrestricted: ties, several roots, inconsistent metadata values.
        /// The same partials in two different orders must give identical rows.
        #[test]
        fn merge_is_independent_of_input_order(bytes in prop::collection::vec(any::<u8>(), 64..512)) {
            let generated = generate(&bytes, false);
            let batches = partial_batches(&generated.groups);
            let first_batches = shuffled(batches.clone(), &bytes);
            let first = merge_all(&first_batches);
            let mut reversed = bytes.iter().copied().rev().collect::<Vec<_>>();
            reversed.extend_from_slice(&bytes);
            let second_batches = shuffled(batches, &reversed);
            let second = merge_all(&second_batches);
            assert_batches_equal(&sort_by_trace_id(&first), &sort_by_trace_id(&second));
        }

        /// For a random requested subset S, the output columns are exactly
        /// S ∪ {trace_id, start_time} and every column equals the full merge.
        #[test]
        fn column_subsets_match_the_full_merge(
            bytes in prop::collection::vec(any::<u8>(), 64..512),
            picks in prop::collection::vec(0..Column::ALL.len(), 0..12),
        ) {
            let generated = generate(&bytes, true);
            let partials = partial_batches(&generated.groups);
            let mut requested: Vec<&str> = picks
                .iter()
                .map(|index| Column::ALL[*index].name())
                .collect();
            requested.sort_unstable();
            requested.dedup();
            let subset = MergeColumns::new(&requested);
            let mut want: Vec<&str> = requested.clone();
            want.push(traces::TRACE_ID);
            want.push(traces::START_TIME);
            want.sort_unstable();
            want.dedup();
            let mut got = subset.output();
            got.sort_unstable();
            prop_assert_eq!(got, want);

            let projected: Vec<Batch> = partials
                .iter()
                .map(|batch| project_columns(batch, &subset.input()))
                .collect();
            let schema = traces_schema();
            let mut merger = SortedTraceMerger::new(&subset, &schema).unwrap();
            if !projected.is_empty() {
                let refs: Vec<&Batch> = projected.iter().collect();
                let proj_schema = Arc::clone(projected[0].schema());
                let combined = Batch::concat(proj_schema, &refs, None).unwrap();
                let sorted = sort_by_trace_id(&combined);
                merger.push_range(&sorted, 0, sorted.raw_len()).unwrap();
            }
            let small = sort_by_trace_id(&merger.finish().unwrap());
            let full = sort_by_trace_id(&merge_all(&partials));
            let big = project_columns(&full, &subset.output());
            assert_batches_equal(&small, &big);
        }
    }

    #[test]
    fn partials_from_spans_batch_handles_encodings_and_selection() {
        let spans = dataset_spans();
        let col_spans = spans_batch(&spans);
        let actual_col = partials_from_spans_batch(&col_spans).unwrap();
        let oracle = trace_batch(&spans);
        assert_batches_equal(&sort_by_trace_id(&actual_col), &sort_by_trace_id(&oracle));

        // Also test empty selection -> empty Batch.
        let empty_masked = col_spans
            .filter_mask(&Bitmap::new_zeroed(col_spans.raw_len()))
            .unwrap();
        assert_eq!(
            partials_from_spans_batch(&empty_masked).unwrap().num_rows(),
            0
        );

        // Test Const, Dict, Rle encodings and Selection::Indices on a small spans batch.
        let mut s1 = base_span(trace_id(1), span_id(1), 100);
        s1.cost_total = Some(0.1);
        s1.span_kind = SpanKind::Llm;
        s1.response_model = Some("gpt-4o".to_owned());
        s1.tags = vec!["alpha".to_owned()];
        s1.metadata.insert("k".to_owned(), "v1".to_owned());
        let mut s2 = base_span(trace_id(1), span_id(2), 200);
        s2.parent_span_id = Some(span_id(1));
        s2.cost_total = Some(0.2);
        s2.tags = vec!["beta".to_owned()];
        s2.metadata.insert("k".to_owned(), "v2".to_owned());
        let s3 = base_span(trace_id(2), span_id(3), 50);

        let base_batch = spans_batch(&[s1, s2, s3]);
        let schema = Arc::clone(base_batch.schema());
        let mut cols = base_batch.columns().to_vec();

        // ProjectId -> Const
        cols[schema.index_of(spans::PROJECT_ID).unwrap()] =
            Arc::new(Col::try_new_const(ScalarValue::Utf8(project().to_string()), 3).unwrap());
        // Status -> Dict
        cols[schema.index_of(spans::STATUS).unwrap()] = Arc::new(Col::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["unset"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![0, 0, 0],
                None,
            )
            .unwrap(),
        ));
        // CostTotal -> Rle
        cols[schema.index_of(spans::COST_TOTAL).unwrap()] = Arc::new(Col::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(FlatData::F64(vec![0.1, 0.2]), None).unwrap(),
                vec![1, 3],
            )
            .unwrap(),
        ));

        let enc_batch =
            Batch::try_new_shared(schema, cols, 3, Selection::Indices(vec![1, 0])).unwrap();
        let enc_partials = partials_from_spans_batch(&enc_batch).unwrap();
        assert_eq!(enc_partials.num_rows(), 1);
        let cost = enc_partials
            .column_by_name(traces::COST_TOTAL)
            .unwrap()
            .get_scalar(0)
            .unwrap();
        assert_eq!(cost, ScalarValue::F64(0.3));
    }
}
