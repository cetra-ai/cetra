//! Convert [`Span`] and [`Score`] slices into columnar batches.
//!
//! This is the ingestion hot path (spec 03 § WAL payload). `duration_ns` and `model`
//! are computed here; they are not fields on [`Span`].

use std::collections::BTreeMap;

use cetra_column::{
    Batch, Bitmap, Column, ColumnError, DataType, EventsData, FlatColumn, FlatData, LinksData,
    MapData, Utf8Data,
};
use uuid::Uuid;

use crate::error::CoreError;
use crate::schema::{scores_schema, spans_schema};
use crate::score::{Score, ScoreTarget};
use crate::span::{Span, SpanEvent, SpanLink};

/// Builds a `spans` batch. An empty slice returns an empty batch with [`spans_schema`].
///
/// Spec 03 § WAL payload and spec 01 § Span.
///
/// # Errors
///
/// Returns [`CoreError::Column`] when a column buffer overflows or fails batch validation.
pub fn spans_to_batch(spans: &[Span]) -> Result<Batch, CoreError> {
    if spans.is_empty() {
        return Ok(Batch::empty(spans_schema()));
    }
    let mut columns = SpanColumns::new(spans);
    for span in spans {
        columns.append(span)?;
    }
    let cols = columns.finish()?;
    Batch::try_new_all(spans_schema(), cols, spans.len()).map_err(CoreError::from)
}

/// Builds a `scores` batch. An empty slice returns an empty batch with [`scores_schema`].
///
/// Spec 03 § WAL payload and spec 01 § Score.
///
/// # Errors
///
/// Returns [`CoreError::Column`] when a column buffer overflows or fails batch validation.
pub fn scores_to_batch(scores: &[Score]) -> Result<Batch, CoreError> {
    if scores.is_empty() {
        return Ok(Batch::empty(scores_schema()));
    }
    let mut columns = ScoreColumns::new(scores);
    for score in scores {
        columns.append(score)?;
    }
    let cols = columns.finish()?;
    Batch::try_new_all(scores_schema(), cols, scores.len()).map_err(CoreError::from)
}

struct SpanColumns {
    project_id: Utf8Builder,
    trace_id: FixedBytes16Builder,
    span_id: FixedBytes8Builder,
    parent_span_id: FixedBytes8Builder,
    name: Utf8Builder,
    span_kind: Utf8Builder,
    start_time: TimestampBuilder,
    end_time: TimestampBuilder,
    duration_ns: I64Builder,
    status: Utf8Builder,
    status_message: Utf8Builder,
    service_name: Utf8Builder,
    session_id: Utf8Builder,
    user_id: Utf8Builder,
    environment: Utf8Builder,
    release: Utf8Builder,
    tags: Utf8ListBuilder,
    metadata: MapBuilder,
    provider: Utf8Builder,
    request_model: Utf8Builder,
    response_model: Utf8Builder,
    model: Utf8Builder,
    prompt_name: Utf8Builder,
    prompt_version: I64Builder,
    operation: Utf8Builder,
    input_tokens: I64Builder,
    output_tokens: I64Builder,
    cache_read_tokens: I64Builder,
    cache_write_tokens: I64Builder,
    reasoning_tokens: I64Builder,
    total_tokens: I64Builder,
    cost_input: F64Builder,
    cost_output: F64Builder,
    cost_total: F64Builder,
    cost_source: Utf8Builder,
    ttft_ns: I64Builder,
    request_params: Utf8Builder,
    input: Utf8Builder,
    input_format: Utf8Builder,
    output: Utf8Builder,
    output_format: Utf8Builder,
    input_preview: Utf8Builder,
    output_preview: Utf8Builder,
    system_instructions: Utf8Builder,
    tool_definitions: Utf8Builder,
    attributes: MapBuilder,
    resource_attributes: MapBuilder,
    scope_name: Utf8Builder,
    scope_version: Utf8Builder,
    events: EventsBuilder,
    links: LinksBuilder,
    media_refs: Utf8ListBuilder,
    truncated: Utf8ListBuilder,
    conventions: Utf8ListBuilder,
    ingest_time: TimestampBuilder,
    schema_version: I64Builder,
}

impl SpanColumns {
    fn new(spans: &[Span]) -> Self {
        let rows = spans.len();
        let input_bytes = sum_len(spans, |span| span.input.as_deref());
        let output_bytes = sum_len(spans, |span| span.output.as_deref());
        let metadata_bytes = map_bytes(spans.iter().map(|span| &span.metadata));
        let attribute_bytes = map_bytes(spans.iter().map(|span| &span.attributes));
        let resource_bytes = map_bytes(spans.iter().map(|span| &span.resource_attributes));
        Self {
            project_id: Utf8Builder::new(rows, rows * 36, false),
            trace_id: FixedBytes16Builder::new(rows, false),
            span_id: FixedBytes8Builder::new(rows, false),
            parent_span_id: FixedBytes8Builder::new(rows, true),
            name: Utf8Builder::new(rows, sum_len(spans, |span| Some(span.name.as_str())), false),
            span_kind: Utf8Builder::new(rows, rows * 16, false),
            start_time: TimestampBuilder::new(rows),
            end_time: TimestampBuilder::new(rows),
            duration_ns: I64Builder::new(rows, false),
            status: Utf8Builder::new(rows, rows * 8, false),
            status_message: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.status_message.as_deref()),
                true,
            ),
            service_name: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.service_name.as_deref()),
                true,
            ),
            session_id: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.session_id.as_deref()),
                true,
            ),
            user_id: Utf8Builder::new(rows, sum_len(spans, |span| span.user_id.as_deref()), true),
            environment: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.environment.as_deref()),
                true,
            ),
            release: Utf8Builder::new(rows, sum_len(spans, |span| span.release.as_deref()), true),
            tags: Utf8ListBuilder::new(rows),
            metadata: MapBuilder::new(rows, metadata_bytes.0, metadata_bytes.1),
            provider: Utf8Builder::new(rows, sum_len(spans, |span| span.provider.as_deref()), true),
            request_model: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.request_model.as_deref()),
                true,
            ),
            response_model: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.response_model.as_deref()),
                true,
            ),
            model: Utf8Builder::new(rows, 0, true),
            prompt_name: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.prompt_name.as_deref()),
                true,
            ),
            prompt_version: I64Builder::new(rows, true),
            operation: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.operation.as_deref()),
                true,
            ),
            input_tokens: I64Builder::new(rows, true),
            output_tokens: I64Builder::new(rows, true),
            cache_read_tokens: I64Builder::new(rows, true),
            cache_write_tokens: I64Builder::new(rows, true),
            reasoning_tokens: I64Builder::new(rows, true),
            total_tokens: I64Builder::new(rows, true),
            cost_input: F64Builder::new(rows),
            cost_output: F64Builder::new(rows),
            cost_total: F64Builder::new(rows),
            cost_source: Utf8Builder::new(rows, rows * 8, true),
            ttft_ns: I64Builder::new(rows, true),
            request_params: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.request_params.as_deref()),
                true,
            ),
            input: Utf8Builder::new(rows, input_bytes, true),
            input_format: Utf8Builder::new(rows, rows * 8, true),
            output: Utf8Builder::new(rows, output_bytes, true),
            output_format: Utf8Builder::new(rows, rows * 8, true),
            input_preview: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.input_preview.as_deref()),
                true,
            ),
            output_preview: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.output_preview.as_deref()),
                true,
            ),
            system_instructions: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.system_instructions.as_deref()),
                true,
            ),
            tool_definitions: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.tool_definitions.as_deref()),
                true,
            ),
            attributes: MapBuilder::new(rows, attribute_bytes.0, attribute_bytes.1),
            resource_attributes: MapBuilder::new(rows, resource_bytes.0, resource_bytes.1),
            scope_name: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.scope_name.as_deref()),
                true,
            ),
            scope_version: Utf8Builder::new(
                rows,
                sum_len(spans, |span| span.scope_version.as_deref()),
                true,
            ),
            events: EventsBuilder::new(rows),
            links: LinksBuilder::new(rows),
            media_refs: Utf8ListBuilder::new(rows),
            truncated: Utf8ListBuilder::new(rows),
            conventions: Utf8ListBuilder::new(rows),
            ingest_time: TimestampBuilder::new(rows),
            schema_version: I64Builder::new(rows, false),
        }
    }

    fn append(&mut self, span: &Span) -> Result<(), CoreError> {
        // No assertions on the producer's invariants (payload with its format, end after
        // start): spans can be read back from disk, and input must never panic (AGENTS.md).
        self.project_id.append(&span.project_id.to_string())?;
        self.trace_id.append(*span.trace_id.as_bytes());
        self.span_id.append(*span.span_id.as_bytes());
        self.parent_span_id
            .append_opt(span.parent_span_id.map(|id| *id.as_bytes()));
        self.name.append(&span.name)?;
        self.span_kind.append(span.span_kind.as_str())?;
        self.start_time.append(span.start_time);
        self.end_time.append(span.end_time);
        // Saturating, like the API's `duration_ns`: extreme instants must not overflow.
        self.duration_ns
            .append(span.end_time.saturating_sub(span.start_time));
        self.status.append(span.status.as_str())?;
        self.status_message
            .append_opt(span.status_message.as_deref())?;
        self.service_name.append_opt(span.service_name.as_deref())?;
        self.session_id.append_opt(span.session_id.as_deref())?;
        self.user_id.append_opt(span.user_id.as_deref())?;
        self.environment.append_opt(span.environment.as_deref())?;
        self.release.append_opt(span.release.as_deref())?;
        self.tags.append_slice(&span.tags)?;
        self.metadata.append(&span.metadata)?;
        self.provider.append_opt(span.provider.as_deref())?;
        self.request_model
            .append_opt(span.request_model.as_deref())?;
        self.response_model
            .append_opt(span.response_model.as_deref())?;
        self.model.append_opt(
            span.response_model
                .as_deref()
                .or(span.request_model.as_deref()),
        )?;
        self.prompt_name.append_opt(span.prompt_name.as_deref())?;
        self.prompt_version.append_opt(span.prompt_version);
        self.operation.append_opt(span.operation.as_deref())?;
        self.input_tokens.append_opt(span.input_tokens);
        self.output_tokens.append_opt(span.output_tokens);
        self.cache_read_tokens.append_opt(span.cache_read_tokens);
        self.cache_write_tokens.append_opt(span.cache_write_tokens);
        self.reasoning_tokens.append_opt(span.reasoning_tokens);
        self.total_tokens.append_opt(span.total_tokens);
        self.cost_input.append_opt(span.cost_input);
        self.cost_output.append_opt(span.cost_output);
        self.cost_total.append_opt(span.cost_total);
        self.cost_source
            .append_opt(span.cost_source.map(|source| source.as_str()))?;
        self.ttft_ns.append_opt(span.ttft_ns);
        self.request_params
            .append_opt(span.request_params.as_deref())?;
        self.input.append_opt(span.input.as_deref())?;
        self.input_format
            .append_opt(span.input_format.map(|format| format.as_str()))?;
        self.output.append_opt(span.output.as_deref())?;
        self.output_format
            .append_opt(span.output_format.map(|format| format.as_str()))?;
        self.input_preview
            .append_opt(span.input_preview.as_deref())?;
        self.output_preview
            .append_opt(span.output_preview.as_deref())?;
        self.system_instructions
            .append_opt(span.system_instructions.as_deref())?;
        self.tool_definitions
            .append_opt(span.tool_definitions.as_deref())?;
        self.attributes.append(&span.attributes)?;
        self.resource_attributes.append(&span.resource_attributes)?;
        self.scope_name.append_opt(span.scope_name.as_deref())?;
        self.scope_version
            .append_opt(span.scope_version.as_deref())?;
        self.events.append(&span.events)?;
        self.links.append(&span.links)?;
        self.media_refs.append_slice(&span.media_refs)?;
        self.truncated.append_slice(&span.truncated)?;
        self.conventions
            .append_iter(span.conventions.iter().map(|item| item.as_str()))?;
        self.ingest_time.append(span.ingest_time);
        self.schema_version.append(i64::from(span.schema_version));
        Ok(())
    }

    fn finish(self) -> Result<Vec<Column>, CoreError> {
        Ok(vec![
            self.project_id.finish()?,
            self.trace_id.finish()?,
            self.span_id.finish()?,
            self.parent_span_id.finish()?,
            self.name.finish()?,
            self.span_kind.finish()?,
            self.start_time.finish()?,
            self.end_time.finish()?,
            self.duration_ns.finish()?,
            self.status.finish()?,
            self.status_message.finish()?,
            self.service_name.finish()?,
            self.session_id.finish()?,
            self.user_id.finish()?,
            self.environment.finish()?,
            self.release.finish()?,
            self.tags.finish()?,
            self.metadata.finish()?,
            self.provider.finish()?,
            self.request_model.finish()?,
            self.response_model.finish()?,
            self.model.finish()?,
            self.prompt_name.finish()?,
            self.prompt_version.finish()?,
            self.operation.finish()?,
            self.input_tokens.finish()?,
            self.output_tokens.finish()?,
            self.cache_read_tokens.finish()?,
            self.cache_write_tokens.finish()?,
            self.reasoning_tokens.finish()?,
            self.total_tokens.finish()?,
            self.cost_input.finish()?,
            self.cost_output.finish()?,
            self.cost_total.finish()?,
            self.cost_source.finish()?,
            self.ttft_ns.finish()?,
            self.request_params.finish()?,
            self.input.finish()?,
            self.input_format.finish()?,
            self.output.finish()?,
            self.output_format.finish()?,
            self.input_preview.finish()?,
            self.output_preview.finish()?,
            self.system_instructions.finish()?,
            self.tool_definitions.finish()?,
            self.attributes.finish()?,
            self.resource_attributes.finish()?,
            self.scope_name.finish()?,
            self.scope_version.finish()?,
            self.events.finish()?,
            self.links.finish()?,
            self.media_refs.finish()?,
            self.truncated.finish()?,
            self.conventions.finish()?,
            self.ingest_time.finish()?,
            self.schema_version.finish()?,
        ])
    }
}

struct ScoreColumns {
    project_id: Utf8Builder,
    score_id: Utf8Builder,
    target: Utf8Builder,
    trace_id: FixedBytes16Builder,
    span_id: FixedBytes8Builder,
    session_id: Utf8Builder,
    user_id: Utf8Builder,
    name: Utf8Builder,
    data_type: Utf8Builder,
    value_num: F64Builder,
    value_str: Utf8Builder,
    reason: Utf8Builder,
    source: Utf8Builder,
    evaluator_id: Utf8Builder,
    rule_id: Utf8Builder,
    experiment_id: Utf8Builder,
    dataset_item_id: Utf8Builder,
    queue_id: Utf8Builder,
    author_user_id: Utf8Builder,
    metadata: MapBuilder,
    created_at: TimestampBuilder,
    updated_at: TimestampBuilder,
    deleted: BoolBuilder,
    ingest_time: TimestampBuilder,
}

impl ScoreColumns {
    fn new(scores: &[Score]) -> Self {
        let rows = scores.len();
        let metadata_bytes = map_bytes(scores.iter().map(|score| &score.metadata));
        Self {
            project_id: Utf8Builder::new(rows, rows * 36, false),
            score_id: Utf8Builder::new(rows, rows * 36, false),
            target: Utf8Builder::new(rows, rows * 8, false),
            trace_id: FixedBytes16Builder::new(rows, true),
            span_id: FixedBytes8Builder::new(rows, true),
            session_id: Utf8Builder::new(
                rows,
                sum_len(scores, |score| score.session_id.as_deref()),
                true,
            ),
            user_id: Utf8Builder::new(
                rows,
                sum_len(scores, |score| score.user_id.as_deref()),
                true,
            ),
            name: Utf8Builder::new(
                rows,
                sum_len(scores, |score| Some(score.name.as_str())),
                false,
            ),
            data_type: Utf8Builder::new(rows, rows * 12, false),
            value_num: F64Builder::new(rows),
            value_str: Utf8Builder::new(rows, 0, true),
            reason: Utf8Builder::new(rows, sum_len(scores, |score| score.reason.as_deref()), true),
            source: Utf8Builder::new(rows, rows * 12, false),
            evaluator_id: Utf8Builder::new(rows, rows * 36, true),
            rule_id: Utf8Builder::new(rows, rows * 36, true),
            experiment_id: Utf8Builder::new(rows, rows * 36, true),
            dataset_item_id: Utf8Builder::new(rows, rows * 36, true),
            queue_id: Utf8Builder::new(rows, rows * 36, true),
            author_user_id: Utf8Builder::new(rows, rows * 36, true),
            metadata: MapBuilder::new(rows, metadata_bytes.0, metadata_bytes.1),
            created_at: TimestampBuilder::new(rows),
            updated_at: TimestampBuilder::new(rows),
            deleted: BoolBuilder::new(),
            ingest_time: TimestampBuilder::new(rows),
        }
    }

    fn append(&mut self, score: &Score) -> Result<(), CoreError> {
        self.project_id.append(&score.project_id.to_string())?;
        self.score_id.append_uuid(score.score_id)?;
        self.target.append(score.target.kind().as_str())?;
        match &score.target {
            ScoreTarget::Span { trace_id, span_id } => {
                self.trace_id.append(*trace_id.as_bytes());
                self.span_id.append(*span_id.as_bytes());
                self.session_id.append_opt(score.session_id.as_deref())?;
            }
            ScoreTarget::Trace { trace_id } => {
                self.trace_id.append(*trace_id.as_bytes());
                self.span_id.append_null();
                self.session_id.append_opt(score.session_id.as_deref())?;
            }
            ScoreTarget::Session { session_id } => {
                self.trace_id.append_null();
                self.span_id.append_null();
                self.session_id.append(session_id)?;
            }
        }
        self.user_id.append_opt(score.user_id.as_deref())?;
        self.name.append(&score.name)?;
        self.data_type.append(score.value.data_type().as_str())?;
        self.value_num.append_opt(score.value.value_num());
        self.value_str.append_opt(score.value.value_str())?;
        self.reason.append_opt(score.reason.as_deref())?;
        self.source.append(score.source.as_str())?;
        self.evaluator_id.append_uuid_opt(score.evaluator_id)?;
        self.rule_id.append_uuid_opt(score.rule_id)?;
        self.experiment_id.append_uuid_opt(score.experiment_id)?;
        self.dataset_item_id
            .append_uuid_opt(score.dataset_item_id)?;
        self.queue_id.append_uuid_opt(score.queue_id)?;
        self.author_user_id.append_uuid_opt(score.author_user_id)?;
        self.metadata.append(&score.metadata)?;
        self.created_at.append(score.created_at);
        self.updated_at.append(score.updated_at);
        self.deleted.append(score.deleted);
        self.ingest_time.append(score.ingest_time);
        Ok(())
    }

    fn finish(self) -> Result<Vec<Column>, CoreError> {
        Ok(vec![
            self.project_id.finish()?,
            self.score_id.finish()?,
            self.target.finish()?,
            self.trace_id.finish()?,
            self.span_id.finish()?,
            self.session_id.finish()?,
            self.user_id.finish()?,
            self.name.finish()?,
            self.data_type.finish()?,
            self.value_num.finish()?,
            self.value_str.finish()?,
            self.reason.finish()?,
            self.source.finish()?,
            self.evaluator_id.finish()?,
            self.rule_id.finish()?,
            self.experiment_id.finish()?,
            self.dataset_item_id.finish()?,
            self.queue_id.finish()?,
            self.author_user_id.finish()?,
            self.metadata.finish()?,
            self.created_at.finish()?,
            self.updated_at.finish()?,
            self.deleted.finish()?,
            self.ingest_time.finish()?,
        ])
    }
}

struct Utf8Builder {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
    nulls: Option<Bitmap>,
}

impl Utf8Builder {
    fn new(rows: usize, byte_cap: usize, nullable: bool) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            offsets,
            bytes: Vec::with_capacity(byte_cap),
            nulls: nullable.then(|| Bitmap::new_zeroed(0)),
        }
    }

    fn append(&mut self, value: &str) -> Result<(), CoreError> {
        let next_len = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(ColumnError::Overflow {
                reason: "utf8 byte length overflow",
            })?;
        let next_u32 = u32::try_from(next_len).map_err(|_| ColumnError::Overflow {
            reason: "utf8 byte length exceeds u32::MAX",
        })?;
        self.bytes.extend_from_slice(value.as_bytes());
        self.offsets.push(next_u32);
        if let Some(nulls) = &mut self.nulls {
            nulls.push(true);
        }
        Ok(())
    }

    fn append_opt(&mut self, value: Option<&str>) -> Result<(), CoreError> {
        match value {
            Some(s) => self.append(s),
            None => {
                let end = *self.offsets.last().unwrap_or(&0);
                self.offsets.push(end);
                if let Some(nulls) = &mut self.nulls {
                    nulls.push(false);
                }
                Ok(())
            }
        }
    }

    fn append_uuid(&mut self, id: Uuid) -> Result<(), CoreError> {
        let mut bytes = [0u8; 36];
        let text = id.hyphenated().encode_lower(&mut bytes);
        self.append(text)
    }

    fn append_uuid_opt(&mut self, id: Option<Uuid>) -> Result<(), CoreError> {
        match id {
            Some(id) => self.append_uuid(id),
            None => self.append_opt(None),
        }
    }

    fn finish_data(self) -> Result<(Utf8Data, Option<Bitmap>), CoreError> {
        let data = Utf8Data::try_new(self.offsets, self.bytes)?;
        Ok((data, self.nulls))
    }

    fn finish(self) -> Result<Column, CoreError> {
        let (data, nulls) = self.finish_data()?;
        FlatColumn::try_new(FlatData::Utf8(data), nulls)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct FixedBytes8Builder {
    values: Vec<[u8; 8]>,
    nulls: Option<Bitmap>,
}

impl FixedBytes8Builder {
    fn new(rows: usize, nullable: bool) -> Self {
        Self {
            values: Vec::with_capacity(rows),
            nulls: nullable.then(|| Bitmap::new_zeroed(0)),
        }
    }

    fn append(&mut self, value: [u8; 8]) {
        self.values.push(value);
        if let Some(nulls) = &mut self.nulls {
            nulls.push(true);
        }
    }

    fn append_null(&mut self) {
        self.values.push([0u8; 8]);
        if let Some(nulls) = &mut self.nulls {
            nulls.push(false);
        }
    }

    fn append_opt(&mut self, value: Option<[u8; 8]>) {
        match value {
            Some(v) => self.append(v),
            None => self.append_null(),
        }
    }

    fn finish(self) -> Result<Column, CoreError> {
        FlatColumn::try_new(FlatData::FixedBytes8(self.values), self.nulls)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct FixedBytes16Builder {
    values: Vec<[u8; 16]>,
    nulls: Option<Bitmap>,
}

impl FixedBytes16Builder {
    fn new(rows: usize, nullable: bool) -> Self {
        Self {
            values: Vec::with_capacity(rows),
            nulls: nullable.then(|| Bitmap::new_zeroed(0)),
        }
    }

    fn append(&mut self, value: [u8; 16]) {
        self.values.push(value);
        if let Some(nulls) = &mut self.nulls {
            nulls.push(true);
        }
    }

    fn append_null(&mut self) {
        self.values.push([0u8; 16]);
        if let Some(nulls) = &mut self.nulls {
            nulls.push(false);
        }
    }

    fn finish(self) -> Result<Column, CoreError> {
        FlatColumn::try_new(FlatData::FixedBytes16(self.values), self.nulls)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct TimestampBuilder {
    values: Vec<i64>,
}

impl TimestampBuilder {
    fn new(rows: usize) -> Self {
        Self {
            values: Vec::with_capacity(rows),
        }
    }

    fn append(&mut self, value: i64) {
        self.values.push(value);
    }

    fn finish(self) -> Result<Column, CoreError> {
        FlatColumn::try_new(
            FlatData::TimestampNs {
                values: self.values,
                utc: true,
            },
            None,
        )
        .map(Column::Flat)
        .map_err(CoreError::from)
    }
}

struct I64Builder {
    values: Vec<i64>,
    nulls: Option<Bitmap>,
}

impl I64Builder {
    fn new(rows: usize, nullable: bool) -> Self {
        Self {
            values: Vec::with_capacity(rows),
            nulls: nullable.then(|| Bitmap::new_zeroed(0)),
        }
    }

    fn append(&mut self, value: i64) {
        self.values.push(value);
        if let Some(nulls) = &mut self.nulls {
            nulls.push(true);
        }
    }

    fn append_opt(&mut self, value: Option<i64>) {
        match value {
            Some(v) => self.append(v),
            None => {
                self.values.push(0);
                if let Some(nulls) = &mut self.nulls {
                    nulls.push(false);
                }
            }
        }
    }

    fn finish(self) -> Result<Column, CoreError> {
        FlatColumn::try_new(FlatData::I64(self.values), self.nulls)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct F64Builder {
    values: Vec<f64>,
    nulls: Bitmap,
}

impl F64Builder {
    fn new(rows: usize) -> Self {
        Self {
            values: Vec::with_capacity(rows),
            nulls: Bitmap::new_zeroed(0),
        }
    }

    fn append_opt(&mut self, value: Option<f64>) {
        match value {
            Some(v) => {
                self.values.push(v);
                self.nulls.push(true);
            }
            None => {
                self.values.push(0.0);
                self.nulls.push(false);
            }
        }
    }

    fn finish(self) -> Result<Column, CoreError> {
        FlatColumn::try_new(FlatData::F64(self.values), Some(self.nulls))
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct BoolBuilder {
    values: Bitmap,
}

impl BoolBuilder {
    fn new() -> Self {
        Self {
            values: Bitmap::new_zeroed(0),
        }
    }

    fn append(&mut self, value: bool) {
        self.values.push(value);
    }

    fn finish(self) -> Result<Column, CoreError> {
        FlatColumn::try_new(FlatData::Bool(self.values), None)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct Utf8ListBuilder {
    offsets: Vec<u32>,
    values: Utf8Builder,
}

impl Utf8ListBuilder {
    fn new(rows: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            offsets,
            values: Utf8Builder::new(rows, 0, false),
        }
    }

    fn append_slice(&mut self, items: &[impl AsRef<str>]) -> Result<(), CoreError> {
        self.append_iter(items.iter().map(AsRef::as_ref))
    }

    fn append_iter<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), CoreError> {
        for item in items {
            self.values.append(item)?;
        }
        let count = self.values.offsets.len() - 1;
        let end = u32::try_from(count).map_err(|_| ColumnError::Overflow {
            reason: "list items exceed u32::MAX",
        })?;
        self.offsets.push(end);
        Ok(())
    }

    fn finish(self) -> Result<Column, CoreError> {
        let (data, _) = self.values.finish_data()?;
        let child = FlatColumn::try_new(FlatData::Utf8(data), None)?;
        let list = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: self.offsets,
                values: Box::new(Column::Flat(child)),
            },
            None,
        )?;
        Ok(Column::Flat(list))
    }
}

struct MapBuilder {
    offsets: Vec<u32>,
    keys: Utf8Builder,
    values: Utf8Builder,
}

impl MapBuilder {
    fn new(rows: usize, key_bytes: usize, value_bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            offsets,
            keys: Utf8Builder::new(rows, key_bytes, false),
            values: Utf8Builder::new(rows, value_bytes, false),
        }
    }

    fn append(&mut self, map: &BTreeMap<String, String>) -> Result<(), CoreError> {
        for (key, value) in map {
            self.keys.append(key)?;
            self.values.append(value)?;
        }
        let count = self.keys.offsets.len() - 1;
        let end = u32::try_from(count).map_err(|_| ColumnError::Overflow {
            reason: "map entries exceed u32::MAX",
        })?;
        self.offsets.push(end);
        Ok(())
    }

    fn finish(self) -> Result<Column, CoreError> {
        let (keys, _) = self.keys.finish_data()?;
        let (values, _) = self.values.finish_data()?;
        let map_data = MapData::try_new(self.offsets, keys, values, None)?;
        FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), None)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct EventsBuilder {
    offsets: Vec<u32>,
    times: Vec<i64>,
    names: Utf8Builder,
    attributes: Utf8Builder,
}

impl EventsBuilder {
    fn new(rows: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            offsets,
            times: Vec::new(),
            names: Utf8Builder::new(0, 0, false),
            attributes: Utf8Builder::new(0, 0, true),
        }
    }

    fn append(&mut self, events: &[SpanEvent]) -> Result<(), CoreError> {
        for event in events {
            self.times.push(event.time);
            self.names.append(&event.name)?;
            self.attributes.append_opt(event.attributes.as_deref())?;
        }
        let end = u32::try_from(self.times.len()).map_err(|_| ColumnError::Overflow {
            reason: "events count exceeds u32::MAX",
        })?;
        self.offsets.push(end);
        Ok(())
    }

    fn finish(self) -> Result<Column, CoreError> {
        let (names, _) = self.names.finish_data()?;
        let (attributes, attribute_nulls) = self.attributes.finish_data()?;
        let events_data =
            EventsData::try_new(self.offsets, self.times, names, attributes, attribute_nulls)?;
        FlatColumn::try_new(FlatData::Events(Box::new(events_data)), None)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

struct LinksBuilder {
    offsets: Vec<u32>,
    trace_ids: Vec<[u8; 16]>,
    span_ids: Vec<[u8; 8]>,
    attributes: Utf8Builder,
}

impl LinksBuilder {
    fn new(rows: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            offsets,
            trace_ids: Vec::new(),
            span_ids: Vec::new(),
            attributes: Utf8Builder::new(0, 0, true),
        }
    }

    fn append(&mut self, links: &[SpanLink]) -> Result<(), CoreError> {
        for link in links {
            self.trace_ids.push(*link.trace_id.as_bytes());
            self.span_ids.push(*link.span_id.as_bytes());
            self.attributes.append_opt(link.attributes.as_deref())?;
        }
        let end = u32::try_from(self.trace_ids.len()).map_err(|_| ColumnError::Overflow {
            reason: "links count exceeds u32::MAX",
        })?;
        self.offsets.push(end);
        Ok(())
    }

    fn finish(self) -> Result<Column, CoreError> {
        let (attributes, attribute_nulls) = self.attributes.finish_data()?;
        let links_data = LinksData::try_new(
            self.offsets,
            self.trace_ids,
            self.span_ids,
            attributes,
            attribute_nulls,
        )?;
        FlatColumn::try_new(FlatData::Links(Box::new(links_data)), None)
            .map(Column::Flat)
            .map_err(CoreError::from)
    }
}

fn sum_len<'a, T>(rows: &'a [T], field: impl Fn(&'a T) -> Option<&'a str>) -> usize {
    rows.iter().filter_map(field).map(str::len).sum()
}

fn map_bytes<'a>(maps: impl Iterator<Item = &'a BTreeMap<String, String>>) -> (usize, usize) {
    let mut keys = 0;
    let mut values = 0;
    for map in maps {
        for (key, value) in map {
            keys += key.len();
            values += value.len();
        }
    }
    (keys, values)
}

#[cfg(test)]
mod tests {
    use cetra_column::frame::{decode_batch, encode_batch};
    use cetra_column::{Batch, Column, FlatColumn, FlatData};

    use super::*;
    use crate::enums::PayloadFormat;
    use crate::ids::{SpanId, TraceId};
    use crate::schema::{self, scores_schema, spans_schema};
    use crate::score::{Score, ScoreTarget, ScoreValue};
    use crate::span::{Span, full_span, minimal_span};

    #[test]
    fn empty_batches_keep_their_schema() {
        let spans = spans_to_batch(&[]).unwrap();
        assert_eq!(spans.num_rows(), 0);
        assert_eq!(spans.schema(), &spans_schema());
        assert_eq!(spans, Batch::empty(spans_schema()));

        let scores = scores_to_batch(&[]).unwrap();
        assert_eq!(scores.num_rows(), 0);
        assert_eq!(scores.schema(), &scores_schema());
        assert_eq!(scores, Batch::empty(scores_schema()));
    }

    #[test]
    fn out_of_order_and_extreme_instants_do_not_panic() {
        let mut reversed = minimal_span();
        reversed.end_time = reversed.start_time - 5;
        let mut extreme = minimal_span();
        extreme.start_time = i64::MIN;
        extreme.end_time = i64::MAX;
        let mut unformatted = minimal_span();
        unformatted.input = Some("hi".to_owned());
        let batch = spans_to_batch(&[reversed, extreme, unformatted]).unwrap();
        assert_eq!(i64_at(&batch, schema::spans::DURATION_NS, 0), -5);
        assert_eq!(i64_at(&batch, schema::spans::DURATION_NS, 1), i64::MAX);
        assert_eq!(batch.num_rows(), 3);
    }

    #[test]
    fn mixed_span_batch_matches_the_schema_and_values() {
        let mut fallback = minimal_span();
        fallback.request_model = Some("requested-only".to_owned());
        let batch = spans_to_batch(&[minimal_span(), full_span(), fallback]).unwrap();
        assert_eq!(batch.schema(), &spans_schema());
        assert!(is_null(&batch, schema::spans::PROMPT_NAME, 0));
        assert!(is_null(&batch, schema::spans::PROMPT_VERSION, 0));

        assert_eq!(utf8(&batch, schema::spans::NAME, 0), Some("root"));
        assert_eq!(utf8(&batch, schema::spans::SPAN_KIND, 0), Some("span"));
        assert_eq!(i64_at(&batch, schema::spans::DURATION_NS, 0), 0);
        assert!(is_null(&batch, schema::spans::MODEL, 0));
        assert!(is_null(&batch, schema::spans::PARENT_SPAN_ID, 0));
        assert_empty_list(&batch, schema::spans::TAGS, 0);
        assert_empty_list(&batch, schema::spans::EVENTS, 0);
        assert_empty_map(&batch, schema::spans::METADATA, 0);
        assert_eq!(i64_at(&batch, schema::spans::SCHEMA_VERSION, 0), 1);
        assert_eq!(
            bin_at(&batch, schema::spans::TRACE_ID, 0),
            Some(minimal_span().trace_id.as_bytes().as_slice())
        );

        let full = full_span();
        assert_eq!(
            i64_at(&batch, schema::spans::DURATION_NS, 1),
            full.end_time - full.start_time
        );
        assert_eq!(
            utf8(&batch, schema::spans::MODEL, 1),
            full.response_model.as_deref()
        );
        assert_eq!(
            utf8(&batch, schema::spans::REQUEST_MODEL, 1),
            full.request_model.as_deref()
        );
        assert_eq!(utf8(&batch, schema::spans::STATUS, 1), Some("error"));
        assert_eq!(utf8(&batch, schema::spans::SPAN_KIND, 1), Some("llm"));
        assert_eq!(i64_at(&batch, schema::spans::INPUT_TOKENS, 1), 10);
        assert_eq!(i64_at(&batch, schema::spans::OUTPUT_TOKENS, 1), 4);
        assert_eq!(i64_at(&batch, schema::spans::CACHE_READ_TOKENS, 1), 2);
        assert_eq!(i64_at(&batch, schema::spans::CACHE_WRITE_TOKENS, 1), 1);
        assert_eq!(i64_at(&batch, schema::spans::REASONING_TOKENS, 1), 3);
        assert_eq!(i64_at(&batch, schema::spans::TOTAL_TOKENS, 1), 14);
        assert_eq!(f64_at(&batch, schema::spans::COST_TOTAL, 1), 0.003);
        assert_eq!(
            utf8(&batch, schema::spans::COST_SOURCE, 1),
            Some("computed")
        );
        assert_eq!(i64_at(&batch, schema::spans::TTFT_NS, 1), 42_000_000);
        assert_eq!(
            utf8(&batch, schema::spans::PROMPT_NAME, 1),
            Some("support-reply")
        );
        assert_eq!(i64_at(&batch, schema::spans::PROMPT_VERSION, 1), 2);
        assert_eq!(list_at(&batch, schema::spans::TAGS, 1), vec!["beta"]);
        assert_eq!(
            list_at(&batch, schema::spans::CONVENTIONS, 1),
            vec!["cetra", "genai"]
        );
        assert_eq!(
            map_at(&batch, schema::spans::METADATA, 1),
            vec![("plan".to_owned(), Some("\"pro\"".to_owned()))]
        );
        assert_eq!(
            map_at(&batch, schema::spans::ATTRIBUTES, 1),
            vec![("http.method".to_owned(), Some("\"GET\"".to_owned()))]
        );
        assert_eq!(event_names(&batch, 1), vec!["exception"]);
        assert_eq!(link_span_ids(&batch, 1), vec![full.links[0].span_id]);
        assert!(!is_null(&batch, schema::spans::EVENTS, 1));

        assert_eq!(
            utf8(&batch, schema::spans::MODEL, 2),
            Some("requested-only")
        );
    }

    #[test]
    fn map_keys_follow_btree_order_and_empty_list_is_not_null() {
        let mut span = minimal_span();
        span.metadata.insert("b".to_owned(), "2".to_owned());
        span.metadata.insert("a".to_owned(), "1".to_owned());
        let batch = spans_to_batch(&[span]).unwrap();
        assert_eq!(
            map_at(&batch, schema::spans::METADATA, 0),
            vec![
                ("a".to_owned(), Some("1".to_owned())),
                ("b".to_owned(), Some("2".to_owned()))
            ]
        );
        assert_empty_list(&batch, schema::spans::TAGS, 0);
        assert!(is_null(&batch, schema::spans::PARENT_SPAN_ID, 0));
    }

    #[test]
    fn score_batch_matches_the_schema_and_flattens_targets() {
        let mut scores = sample_scores();
        scores[2].session_id = Some("ignored".to_owned());
        let batch = scores_to_batch(&scores).unwrap();
        assert_eq!(batch.schema(), &scores_schema());
        assert_eq!(utf8(&batch, schema::scores::TARGET, 0), Some("span"));
        assert!(bin_at(&batch, schema::scores::SPAN_ID, 0).is_some());
        assert_eq!(
            utf8(&batch, schema::scores::SESSION_ID, 0),
            Some("from-field")
        );
        assert_eq!(utf8(&batch, schema::scores::DATA_TYPE, 0), Some("numeric"));
        assert_eq!(f64_at(&batch, schema::scores::VALUE_NUM, 0), 0.8);
        assert!(is_null(&batch, schema::scores::VALUE_STR, 0));

        assert_eq!(utf8(&batch, schema::scores::TARGET, 1), Some("trace"));
        assert!(is_null(&batch, schema::scores::SPAN_ID, 1));
        assert!(bin_at(&batch, schema::scores::TRACE_ID, 1).is_some());
        assert_eq!(utf8(&batch, schema::scores::DATA_TYPE, 1), Some("boolean"));
        assert_eq!(f64_at(&batch, schema::scores::VALUE_NUM, 1), 0.0);

        assert_eq!(utf8(&batch, schema::scores::TARGET, 2), Some("session"));
        assert!(is_null(&batch, schema::scores::TRACE_ID, 2));
        assert_eq!(
            utf8(&batch, schema::scores::SESSION_ID, 2),
            Some("from-target")
        );
        assert_eq!(utf8(&batch, schema::scores::VALUE_STR, 2), Some("correct"));
        assert!(!bool_at(&batch, schema::scores::DELETED, 2));
        assert_eq!(
            map_at(&batch, schema::scores::METADATA, 2),
            vec![("judge.model".to_owned(), Some("gpt-5-mini".to_owned()))]
        );
    }

    #[test]
    fn score_batch_writes_every_optional_column() {
        let full = full_score();
        let batch = scores_to_batch(std::slice::from_ref(&full)).unwrap();
        assert_eq!(batch.schema(), &scores_schema());
        let uuid_text = |id: Option<Uuid>| id.unwrap().hyphenated().to_string();
        assert_eq!(
            utf8(&batch, schema::scores::PROJECT_ID, 0),
            Some("018f6b4e-7c1a-7c1a-8c1a-000000000001")
        );
        assert_eq!(
            utf8(&batch, schema::scores::SCORE_ID, 0),
            Some("018f6b4e-7c1a-7c1a-8c1a-0000000000ab")
        );
        assert_eq!(utf8(&batch, schema::scores::USER_ID, 0), Some("user-7"));
        assert_eq!(utf8(&batch, schema::scores::NAME, 0), Some("accuracy"));
        assert_eq!(utf8(&batch, schema::scores::REASON, 0), Some("matches"));
        assert_eq!(utf8(&batch, schema::scores::SOURCE, 0), Some("human"));
        for (column, id) in [
            (schema::scores::EVALUATOR_ID, full.evaluator_id),
            (schema::scores::RULE_ID, full.rule_id),
            (schema::scores::EXPERIMENT_ID, full.experiment_id),
            (schema::scores::DATASET_ITEM_ID, full.dataset_item_id),
            (schema::scores::QUEUE_ID, full.queue_id),
            (schema::scores::AUTHOR_USER_ID, full.author_user_id),
        ] {
            let text = uuid_text(id);
            assert_eq!(utf8(&batch, column, 0), Some(text.as_str()), "{column}");
        }
        for (column, expected) in [
            (schema::scores::CREATED_AT, full.created_at),
            (schema::scores::UPDATED_AT, full.updated_at),
            (schema::scores::INGEST_TIME, full.ingest_time),
        ] {
            assert_eq!(column_ts(&batch, column), expected, "{column}");
        }
        assert!(bool_at(&batch, schema::scores::DELETED, 0));
    }

    #[test]
    fn frame_round_trip() {
        let spans = spans_to_batch(&[minimal_span(), full_span()]).unwrap();
        assert_eq!(spans, round_trip_frame(&spans));
        let scores = scores_to_batch(&sample_scores()).unwrap();
        assert_eq!(scores, round_trip_frame(&scores));
    }

    #[test]
    #[ignore]
    fn convert_100k_spans() {
        let payload = "x".repeat(2048);
        let spans: Vec<Span> = (0..100_000)
            .map(|index| {
                let mut span = minimal_span();
                span.name = format!("span-{index}");
                span.start_time = index;
                span.end_time = index + 1_000;
                span.input = Some(payload.clone());
                span.output = Some(payload.clone());
                span.input_format = Some(PayloadFormat::Text);
                span.output_format = Some(PayloadFormat::Text);
                span
            })
            .collect();
        let started = std::time::Instant::now();
        let batch = spans_to_batch(&spans).unwrap();
        let elapsed = started.elapsed();
        println!("converted {} spans in {elapsed:?}", batch.num_rows());
    }

    fn round_trip_frame(batch: &Batch) -> Batch {
        let mut bytes = Vec::new();
        encode_batch(batch, &mut bytes, None).unwrap();
        decode_batch(&bytes, Some(batch.schema().as_ref()), None).unwrap()
    }

    fn column_ts(batch: &Batch, name: &str) -> i64 {
        let flat = flat_col(batch, name);
        assert!(!flat.is_null(0));
        match flat.data() {
            FlatData::TimestampNs { values, utc } => {
                assert!(*utc);
                values[0]
            }
            other => panic!(
                "expected TimestampNs for {name}, got {:?}",
                other.data_type()
            ),
        }
    }

    /// A score with every optional field set. Its `score_id` is parsed from upper case to check
    /// the column is written lower case.
    fn full_score() -> Score {
        let mut score = score(
            ScoreTarget::Trace { trace_id: trace() },
            ScoreValue::Numeric(0.25),
            None,
        );
        score.score_id = "018F6B4E-7C1A-7C1A-8C1A-0000000000AB".parse().unwrap();
        score.user_id = Some("user-7".to_owned());
        score.reason = Some("matches".to_owned());
        score.source = crate::enums::ScoreSource::Human;
        score.evaluator_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e1".parse().unwrap());
        score.rule_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e2".parse().unwrap());
        score.experiment_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e3".parse().unwrap());
        score.dataset_item_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e4".parse().unwrap());
        score.queue_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e6".parse().unwrap());
        score.author_user_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e5".parse().unwrap());
        score.created_at = 1_767_261_600_000_000_000;
        score.updated_at = 1_767_261_601_000_000_000;
        score.ingest_time = 1_767_261_602_000_000_000;
        score.deleted = true;
        score
    }

    fn sample_scores() -> Vec<Score> {
        vec![
            score(
                ScoreTarget::Span {
                    trace_id: trace(),
                    span_id: span_id(),
                },
                ScoreValue::Numeric(0.8),
                Some("from-field"),
            ),
            score(
                ScoreTarget::Trace { trace_id: trace() },
                ScoreValue::Boolean(false),
                None,
            ),
            score(
                ScoreTarget::Session {
                    session_id: "from-target".to_owned(),
                },
                ScoreValue::Categorical {
                    label: "correct".to_owned(),
                    value: Some(1.0),
                },
                Some("from-target"),
            ),
            full_score(),
        ]
    }

    fn score(target: ScoreTarget, value: ScoreValue, session_id: Option<&str>) -> Score {
        let mut metadata = BTreeMap::new();
        metadata.insert("judge.model".to_owned(), "gpt-5-mini".to_owned());
        Score {
            project_id: "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap(),
            score_id: "018f6b4e-7c1a-7c1a-8c1a-000000000002".parse().unwrap(),
            target,
            session_id: session_id.map(str::to_owned),
            user_id: None,
            name: "accuracy".to_owned(),
            value,
            reason: None,
            source: crate::enums::ScoreSource::Api,
            evaluator_id: None,
            rule_id: None,
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata,
            created_at: 0,
            updated_at: 0,
            deleted: false,
            ingest_time: 0,
        }
    }

    fn trace() -> TraceId {
        "4bf92f3577b34da6a3ce929d0e0e4736".parse().unwrap()
    }

    fn span_id() -> SpanId {
        "00f067aa0ba902b7".parse().unwrap()
    }

    fn flat_col<'a>(batch: &'a Batch, name: &str) -> &'a FlatColumn {
        match batch
            .column_by_name(name)
            .unwrap_or_else(|| panic!("missing column {name}"))
        {
            Column::Flat(fc) => fc,
            _ => panic!("expected Flat column for {name}"),
        }
    }

    fn is_null(batch: &Batch, name: &str, row: usize) -> bool {
        flat_col(batch, name).is_null(row)
    }

    fn utf8<'a>(batch: &'a Batch, name: &str, row: usize) -> Option<&'a str> {
        let flat = flat_col(batch, name);
        if flat.is_null(row) {
            return None;
        }
        match flat.data() {
            FlatData::Utf8(u) => u.get(row),
            _ => panic!("expected Utf8 for {name}"),
        }
    }

    fn i64_at(batch: &Batch, name: &str, row: usize) -> i64 {
        let flat = flat_col(batch, name);
        assert!(!flat.is_null(row), "{name} row {row} is null");
        match flat.data() {
            FlatData::I64(v) => v[row],
            _ => panic!("expected I64 for {name}"),
        }
    }

    fn f64_at(batch: &Batch, name: &str, row: usize) -> f64 {
        let flat = flat_col(batch, name);
        assert!(!flat.is_null(row), "{name} row {row} is null");
        match flat.data() {
            FlatData::F64(v) => v[row],
            _ => panic!("expected F64 for {name}"),
        }
    }

    fn bool_at(batch: &Batch, name: &str, row: usize) -> bool {
        let flat = flat_col(batch, name);
        assert!(!flat.is_null(row), "{name} row {row} is null");
        match flat.data() {
            FlatData::Bool(bm) => bm.get(row),
            _ => panic!("expected Bool for {name}"),
        }
    }

    fn bin_at<'a>(batch: &'a Batch, name: &str, row: usize) -> Option<&'a [u8]> {
        let flat = flat_col(batch, name);
        if flat.is_null(row) {
            return None;
        }
        match flat.data() {
            FlatData::FixedBytes8(v) => Some(&v[row]),
            FlatData::FixedBytes16(v) => Some(&v[row]),
            _ => panic!("expected FixedBytes for {name}"),
        }
    }

    fn list_at(batch: &Batch, name: &str, row: usize) -> Vec<String> {
        let flat = flat_col(batch, name);
        assert!(!flat.is_null(row), "{name} is null");
        match flat.data() {
            FlatData::List {
                offsets, values, ..
            } => {
                let Column::Flat(child) = values.as_ref() else {
                    panic!("expected Flat list child for {name}");
                };
                let FlatData::Utf8(u) = child.data() else {
                    panic!("expected Utf8 list child for {name}");
                };
                let start = offsets[row] as usize;
                let end = offsets[row + 1] as usize;
                (start..end).map(|i| u.get(i).unwrap().to_owned()).collect()
            }
            _ => panic!("expected List for {name}"),
        }
    }

    fn assert_empty_list(batch: &Batch, name: &str, row: usize) {
        let flat = flat_col(batch, name);
        assert!(!flat.is_null(row));
        match flat.data() {
            FlatData::List { offsets, .. } => {
                assert_eq!(offsets[row], offsets[row + 1]);
            }
            FlatData::Events(e) => {
                assert!(e.get(row).unwrap().is_empty());
            }
            FlatData::Links(l) => {
                assert!(l.get(row).unwrap().is_empty());
            }
            _ => panic!("expected List/Events/Links for {name}"),
        }
    }

    fn map_at(batch: &Batch, name: &str, row: usize) -> Vec<(String, Option<String>)> {
        let flat = flat_col(batch, name);
        assert!(!flat.is_null(row));
        match flat.data() {
            FlatData::MapUtf8(m) => m.get(row).unwrap(),
            _ => panic!("expected MapUtf8 for {name}"),
        }
    }

    fn assert_empty_map(batch: &Batch, name: &str, row: usize) {
        assert!(map_at(batch, name, row).is_empty());
    }

    fn event_names(batch: &Batch, row: usize) -> Vec<String> {
        let flat = flat_col(batch, schema::spans::EVENTS);
        match flat.data() {
            FlatData::Events(e) => e.get(row).unwrap().into_iter().map(|ev| ev.name).collect(),
            _ => panic!("expected Events"),
        }
    }

    fn link_span_ids(batch: &Batch, row: usize) -> Vec<SpanId> {
        let flat = flat_col(batch, schema::spans::LINKS);
        match flat.data() {
            FlatData::Links(l) => l
                .get(row)
                .unwrap()
                .into_iter()
                .map(|lk| SpanId::from_bytes(lk.span_id).unwrap())
                .collect(),
            _ => panic!("expected Links"),
        }
    }
}
