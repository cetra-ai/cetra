use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use arrow::array::{
    ArrayRef, BooleanBuilder, FixedSizeBinaryBuilder, Float64Builder, Int64Builder, ListBuilder,
    MapBuilder, RecordBatch, StringBuilder, StructBuilder, TimestampNanosecondBuilder,
    UInt16Builder,
};
use arrow::datatypes::{
    DataType as ArrowType, Field as ArrowField, Fields as ArrowFields, Schema as ArrowSchema,
    SchemaRef, TimeUnit,
};
use cetra_column::{
    Batch, BinaryData, Bitmap, Column, DataType as ColType, EventItem, EventsData,
    Field as ColField, FlatColumn, FlatData, LinkItem, LinksData, MapData, Schema as ColSchema,
    Utf8Data,
};
use parquet::arrow::ArrowSchemaConverter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::SortingColumn;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::schema::types::{ColumnPath, SchemaDescriptor};

pub(crate) type BenchResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) const SPANS_DICTIONARY_COLUMNS: &[&str] = &[
    "project_id",
    "span_kind",
    "status",
    "service_name",
    "environment",
    "release",
    "provider",
    "request_model",
    "response_model",
    "model",
    "prompt_name",
    "operation",
    "cost_source",
    "input_format",
    "output_format",
    "scope_name",
    "scope_version",
    "conventions",
];

pub(crate) const SCORES_DICTIONARY_COLUMNS: &[&str] = &["target", "name", "data_type", "source"];

pub(crate) const SPANS_BLOOM_COLUMNS: &[&str] = &["trace_id", "session_id", "user_id"];
pub(crate) const SCORES_BLOOM_COLUMNS: &[&str] = &["trace_id", "score_id"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableKindBench {
    Spans,
    Scores,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParquetVariant {
    Default,
    Tuned,
    Cetra,
}

#[derive(Clone)]
pub(crate) struct SpanEventRow {
    pub(crate) time: i64,
    pub(crate) name: String,
    pub(crate) attributes: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SpanLinkRow {
    pub(crate) trace_id: [u8; 16],
    pub(crate) span_id: [u8; 8],
    pub(crate) attributes: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SpanRow {
    pub(crate) project_id: String,
    pub(crate) trace_id: [u8; 16],
    pub(crate) span_id: [u8; 8],
    pub(crate) parent_span_id: Option<[u8; 8]>,
    pub(crate) name: String,
    pub(crate) span_kind: String,
    pub(crate) start_time: i64,
    pub(crate) end_time: i64,
    pub(crate) duration_ns: i64,
    pub(crate) status: String,
    pub(crate) status_message: Option<String>,
    pub(crate) service_name: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<String>,
    pub(crate) environment: Option<String>,
    pub(crate) release: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) metadata: BTreeMap<String, Option<String>>,
    pub(crate) provider: Option<String>,
    pub(crate) request_model: Option<String>,
    pub(crate) response_model: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) prompt_name: Option<String>,
    pub(crate) prompt_version: Option<i64>,
    pub(crate) operation: Option<String>,
    pub(crate) input_tokens: Option<i64>,
    pub(crate) output_tokens: Option<i64>,
    pub(crate) cache_read_tokens: Option<i64>,
    pub(crate) cache_write_tokens: Option<i64>,
    pub(crate) reasoning_tokens: Option<i64>,
    pub(crate) total_tokens: Option<i64>,
    pub(crate) cost_input: Option<f64>,
    pub(crate) cost_output: Option<f64>,
    pub(crate) cost_total: Option<f64>,
    pub(crate) cost_source: Option<String>,
    pub(crate) ttft_ns: Option<i64>,
    pub(crate) request_params: Option<String>,
    pub(crate) input: Option<String>,
    pub(crate) input_format: Option<String>,
    pub(crate) output: Option<String>,
    pub(crate) output_format: Option<String>,
    pub(crate) input_preview: Option<String>,
    pub(crate) output_preview: Option<String>,
    pub(crate) system_instructions: Option<String>,
    pub(crate) tool_definitions: Option<String>,
    pub(crate) attributes: BTreeMap<String, Option<String>>,
    pub(crate) resource_attributes: BTreeMap<String, Option<String>>,
    pub(crate) scope_name: Option<String>,
    pub(crate) scope_version: Option<String>,
    pub(crate) events: Vec<SpanEventRow>,
    pub(crate) links: Vec<SpanLinkRow>,
    pub(crate) media_refs: Vec<String>,
    pub(crate) truncated: Vec<String>,
    pub(crate) conventions: Vec<String>,
    pub(crate) ingest_time: i64,
    pub(crate) schema_version: u16,
}

#[derive(Clone)]
pub(crate) struct ScoreRow {
    pub(crate) project_id: String,
    pub(crate) score_id: String,
    pub(crate) target: String,
    pub(crate) trace_id: Option<[u8; 16]>,
    pub(crate) span_id: Option<[u8; 8]>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<String>,
    pub(crate) name: String,
    pub(crate) data_type: String,
    pub(crate) value_num: Option<f64>,
    pub(crate) value_str: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) source: String,
    pub(crate) evaluator_id: Option<String>,
    pub(crate) rule_id: Option<String>,
    pub(crate) experiment_id: Option<String>,
    pub(crate) dataset_item_id: Option<String>,
    pub(crate) queue_id: Option<String>,
    pub(crate) author_user_id: Option<String>,
    pub(crate) metadata: BTreeMap<String, Option<String>>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) deleted: bool,
    pub(crate) ingest_time: i64,
}

fn arrow_ts() -> ArrowType {
    ArrowType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))
}

fn arrow_utf8_list() -> ArrowType {
    ArrowType::List(Arc::new(ArrowField::new("item", ArrowType::Utf8, false)))
}

fn arrow_map_utf8() -> ArrowType {
    let entries = ArrowField::new(
        "entries",
        ArrowType::Struct(ArrowFields::from(vec![
            ArrowField::new("keys", ArrowType::Utf8, false),
            ArrowField::new("values", ArrowType::Utf8, true),
        ])),
        false,
    );
    ArrowType::Map(Arc::new(entries), false)
}

fn arrow_event_fields() -> ArrowFields {
    ArrowFields::from(vec![
        ArrowField::new("time", arrow_ts(), false),
        ArrowField::new("name", ArrowType::Utf8, false),
        ArrowField::new("attributes", ArrowType::Utf8, true),
    ])
}

fn arrow_link_fields() -> ArrowFields {
    ArrowFields::from(vec![
        ArrowField::new("trace_id", ArrowType::FixedSizeBinary(16), false),
        ArrowField::new("span_id", ArrowType::FixedSizeBinary(8), false),
        ArrowField::new("attributes", ArrowType::Utf8, true),
    ])
}

fn arrow_struct_list(fields: ArrowFields) -> ArrowType {
    ArrowType::List(Arc::new(ArrowField::new(
        "item",
        ArrowType::Struct(fields),
        false,
    )))
}

static ARROW_SPANS_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    let f = |n: &str, t: ArrowType, nu: bool| ArrowField::new(n, t, nu);
    Arc::new(ArrowSchema::new(vec![
        f("project_id", ArrowType::Utf8, false),
        f("trace_id", ArrowType::FixedSizeBinary(16), false),
        f("span_id", ArrowType::FixedSizeBinary(8), false),
        f("parent_span_id", ArrowType::FixedSizeBinary(8), true),
        f("name", ArrowType::Utf8, false),
        f("span_kind", ArrowType::Utf8, false),
        f("start_time", arrow_ts(), false),
        f("end_time", arrow_ts(), false),
        f("duration_ns", ArrowType::Int64, false),
        f("status", ArrowType::Utf8, false),
        f("status_message", ArrowType::Utf8, true),
        f("service_name", ArrowType::Utf8, true),
        f("session_id", ArrowType::Utf8, true),
        f("user_id", ArrowType::Utf8, true),
        f("environment", ArrowType::Utf8, true),
        f("release", ArrowType::Utf8, true),
        f("tags", arrow_utf8_list(), false),
        f("metadata", arrow_map_utf8(), false),
        f("provider", ArrowType::Utf8, true),
        f("request_model", ArrowType::Utf8, true),
        f("response_model", ArrowType::Utf8, true),
        f("model", ArrowType::Utf8, true),
        f("prompt_name", ArrowType::Utf8, true),
        f("prompt_version", ArrowType::Int64, true),
        f("operation", ArrowType::Utf8, true),
        f("input_tokens", ArrowType::Int64, true),
        f("output_tokens", ArrowType::Int64, true),
        f("cache_read_tokens", ArrowType::Int64, true),
        f("cache_write_tokens", ArrowType::Int64, true),
        f("reasoning_tokens", ArrowType::Int64, true),
        f("total_tokens", ArrowType::Int64, true),
        f("cost_input", ArrowType::Float64, true),
        f("cost_output", ArrowType::Float64, true),
        f("cost_total", ArrowType::Float64, true),
        f("cost_source", ArrowType::Utf8, true),
        f("ttft_ns", ArrowType::Int64, true),
        f("request_params", ArrowType::Utf8, true),
        f("input", ArrowType::Utf8, true),
        f("input_format", ArrowType::Utf8, true),
        f("output", ArrowType::Utf8, true),
        f("output_format", ArrowType::Utf8, true),
        f("input_preview", ArrowType::Utf8, true),
        f("output_preview", ArrowType::Utf8, true),
        f("system_instructions", ArrowType::Utf8, true),
        f("tool_definitions", ArrowType::Utf8, true),
        f("attributes", arrow_map_utf8(), false),
        f("resource_attributes", arrow_map_utf8(), false),
        f("scope_name", ArrowType::Utf8, true),
        f("scope_version", ArrowType::Utf8, true),
        f("events", arrow_struct_list(arrow_event_fields()), false),
        f("links", arrow_struct_list(arrow_link_fields()), false),
        f("media_refs", arrow_utf8_list(), false),
        f("truncated", arrow_utf8_list(), false),
        f("conventions", arrow_utf8_list(), false),
        f("ingest_time", arrow_ts(), false),
        f("schema_version", ArrowType::UInt16, false),
    ]))
});

static ARROW_SCORES_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    let f = |n: &str, t: ArrowType, nu: bool| ArrowField::new(n, t, nu);
    Arc::new(ArrowSchema::new(vec![
        f("project_id", ArrowType::Utf8, false),
        f("score_id", ArrowType::Utf8, false),
        f("target", ArrowType::Utf8, false),
        f("trace_id", ArrowType::FixedSizeBinary(16), true),
        f("span_id", ArrowType::FixedSizeBinary(8), true),
        f("session_id", ArrowType::Utf8, true),
        f("user_id", ArrowType::Utf8, true),
        f("name", ArrowType::Utf8, false),
        f("data_type", ArrowType::Utf8, false),
        f("value_num", ArrowType::Float64, true),
        f("value_str", ArrowType::Utf8, true),
        f("reason", ArrowType::Utf8, true),
        f("source", ArrowType::Utf8, false),
        f("evaluator_id", ArrowType::Utf8, true),
        f("rule_id", ArrowType::Utf8, true),
        f("experiment_id", ArrowType::Utf8, true),
        f("dataset_item_id", ArrowType::Utf8, true),
        f("queue_id", ArrowType::Utf8, true),
        f("author_user_id", ArrowType::Utf8, true),
        f("metadata", arrow_map_utf8(), false),
        f("created_at", arrow_ts(), false),
        f("updated_at", arrow_ts(), false),
        f("deleted", ArrowType::Boolean, false),
        f("ingest_time", arrow_ts(), false),
    ]))
});

static COL_SPANS_SCHEMA: LazyLock<Arc<ColSchema>> = LazyLock::new(|| {
    let ts = ColType::TimestampNs { utc: true };
    let utf8_list = ColType::List(Box::new(ColType::Utf8));
    let f = |n: &str, t: ColType, nu: bool| ColField::new(n, t, nu);
    ColSchema::try_new_arc(vec![
        f("project_id", ColType::Utf8, false),
        f("trace_id", ColType::FixedBytes16, false),
        f("span_id", ColType::FixedBytes8, false),
        f("parent_span_id", ColType::FixedBytes8, true),
        f("name", ColType::Utf8, false),
        f("span_kind", ColType::Utf8, false),
        f("start_time", ts.clone(), false),
        f("end_time", ts.clone(), false),
        f("duration_ns", ColType::I64, false),
        f("status", ColType::Utf8, false),
        f("status_message", ColType::Utf8, true),
        f("service_name", ColType::Utf8, true),
        f("session_id", ColType::Utf8, true),
        f("user_id", ColType::Utf8, true),
        f("environment", ColType::Utf8, true),
        f("release", ColType::Utf8, true),
        f("tags", utf8_list.clone(), false),
        f("metadata", ColType::MapUtf8, false),
        f("provider", ColType::Utf8, true),
        f("request_model", ColType::Utf8, true),
        f("response_model", ColType::Utf8, true),
        f("model", ColType::Utf8, true),
        f("prompt_name", ColType::Utf8, true),
        f("prompt_version", ColType::I64, true),
        f("operation", ColType::Utf8, true),
        f("input_tokens", ColType::I64, true),
        f("output_tokens", ColType::I64, true),
        f("cache_read_tokens", ColType::I64, true),
        f("cache_write_tokens", ColType::I64, true),
        f("reasoning_tokens", ColType::I64, true),
        f("total_tokens", ColType::I64, true),
        f("cost_input", ColType::F64, true),
        f("cost_output", ColType::F64, true),
        f("cost_total", ColType::F64, true),
        f("cost_source", ColType::Utf8, true),
        f("ttft_ns", ColType::I64, true),
        f("request_params", ColType::Utf8, true),
        f("input", ColType::Utf8, true),
        f("input_format", ColType::Utf8, true),
        f("output", ColType::Utf8, true),
        f("output_format", ColType::Utf8, true),
        f("input_preview", ColType::Utf8, true),
        f("output_preview", ColType::Utf8, true),
        f("system_instructions", ColType::Utf8, true),
        f("tool_definitions", ColType::Utf8, true),
        f("attributes", ColType::MapUtf8, false),
        f("resource_attributes", ColType::MapUtf8, false),
        f("scope_name", ColType::Utf8, true),
        f("scope_version", ColType::Utf8, true),
        f("events", ColType::Events, false),
        f("links", ColType::Links, false),
        f("media_refs", utf8_list.clone(), false),
        f("truncated", utf8_list.clone(), false),
        f("conventions", utf8_list, false),
        f("ingest_time", ts, false),
        f("schema_version", ColType::I64, false),
    ])
    .unwrap_or_else(|e| panic!("invalid spans col schema: {e}"))
});

static COL_SCORES_SCHEMA: LazyLock<Arc<ColSchema>> = LazyLock::new(|| {
    let ts = ColType::TimestampNs { utc: true };
    let f = |n: &str, t: ColType, nu: bool| ColField::new(n, t, nu);
    ColSchema::try_new_arc(vec![
        f("project_id", ColType::Utf8, false),
        f("score_id", ColType::Utf8, false),
        f("target", ColType::Utf8, false),
        f("trace_id", ColType::FixedBytes16, true),
        f("span_id", ColType::FixedBytes8, true),
        f("session_id", ColType::Utf8, true),
        f("user_id", ColType::Utf8, true),
        f("name", ColType::Utf8, false),
        f("data_type", ColType::Utf8, false),
        f("value_num", ColType::F64, true),
        f("value_str", ColType::Utf8, true),
        f("reason", ColType::Utf8, true),
        f("source", ColType::Utf8, false),
        f("evaluator_id", ColType::Utf8, true),
        f("rule_id", ColType::Utf8, true),
        f("experiment_id", ColType::Utf8, true),
        f("dataset_item_id", ColType::Utf8, true),
        f("queue_id", ColType::Utf8, true),
        f("author_user_id", ColType::Utf8, true),
        f("metadata", ColType::MapUtf8, false),
        f("created_at", ts.clone(), false),
        f("updated_at", ts.clone(), false),
        f("deleted", ColType::Bool, false),
        f("ingest_time", ts, false),
    ])
    .unwrap_or_else(|e| panic!("invalid scores col schema: {e}"))
});

pub(crate) fn arrow_spans_schema() -> SchemaRef {
    Arc::clone(&ARROW_SPANS_SCHEMA)
}

pub(crate) fn arrow_scores_schema() -> SchemaRef {
    Arc::clone(&ARROW_SCORES_SCHEMA)
}

pub(crate) fn col_spans_schema() -> Arc<ColSchema> {
    Arc::clone(&COL_SPANS_SCHEMA)
}

pub(crate) fn col_scores_schema() -> Arc<ColSchema> {
    Arc::clone(&COL_SCORES_SCHEMA)
}

pub(crate) fn sort_spans(rows: &mut [SpanRow]) {
    rows.sort_by(|a, b| {
        a.trace_id
            .cmp(&b.trace_id)
            .then(a.start_time.cmp(&b.start_time))
    });
}

pub(crate) fn sort_scores(rows: &mut [ScoreRow]) {
    rows.sort_by_key(|r| r.created_at);
}

pub(crate) fn parquet_writer_props(
    variant: ParquetVariant,
    table: TableKindBench,
    schema: &ArrowSchema,
    rows: usize,
) -> BenchResult<WriterProperties> {
    let zstd = Compression::ZSTD(ZstdLevel::try_new(3)?);
    let descriptor = ArrowSchemaConverter::new().convert(schema)?;
    let bloom_cols = match table {
        TableKindBench::Spans => SPANS_BLOOM_COLUMNS,
        TableKindBench::Scores => SCORES_BLOOM_COLUMNS,
    };

    match variant {
        ParquetVariant::Default => Ok(WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_compression(zstd)
            .set_max_row_group_row_count(Some(1_000_000))
            .set_data_page_size_limit(1 << 20)
            .set_data_page_row_count_limit(20_000)
            .set_statistics_enabled(EnabledStatistics::Page)
            .set_offset_index_disabled(false)
            .set_dictionary_enabled(true)
            .build()),
        ParquetVariant::Tuned => {
            let mut b = WriterProperties::builder()
                .set_writer_version(WriterVersion::PARQUET_2_0)
                .set_compression(zstd)
                .set_max_row_group_row_count(Some(128 * 1024))
                .set_data_page_size_limit(64 << 10)
                .set_data_page_row_count_limit(8_192)
                .set_statistics_enabled(EnabledStatistics::Page)
                .set_offset_index_disabled(false);
            let ndv = (rows / 4).clamp(1, 131_072) as u64;
            for name in bloom_cols {
                if let Some(p) = find_leaf_path(&descriptor, name) {
                    b = b
                        .set_column_bloom_filter_enabled(p.clone(), true)
                        .set_column_bloom_filter_fpp(p.clone(), 0.01)
                        .set_column_bloom_filter_max_ndv(p, ndv);
                }
            }
            for name in [
                "input",
                "output",
                "system_instructions",
                "tool_definitions",
                "attributes",
                "resource_attributes",
                "metadata",
                "input_preview",
                "output_preview",
                "events",
                "links",
                "reason",
                "value_str",
            ] {
                for c in descriptor.columns() {
                    if c.path().parts().first().map(String::as_str) == Some(name) {
                        b = b.set_column_dictionary_enabled(c.path().clone(), false);
                    }
                }
            }
            Ok(b.build())
        }
        ParquetVariant::Cetra => {
            let sort_cols = match table {
                TableKindBench::Spans => vec!["trace_id", "start_time"],
                TableKindBench::Scores => vec!["created_at"],
            };
            let mut sorting = Vec::new();
            for name in sort_cols {
                if let Some(idx) = find_leaf_index(&descriptor, name) {
                    sorting.push(SortingColumn {
                        column_idx: idx,
                        descending: false,
                        nulls_first: false,
                    });
                }
            }
            let mut b = WriterProperties::builder()
                .set_writer_version(WriterVersion::PARQUET_1_0)
                .set_compression(zstd)
                .set_dictionary_enabled(false)
                .set_data_page_size_limit(1 << 20)
                .set_data_page_row_count_limit(20_000)
                .set_statistics_enabled(EnabledStatistics::Page)
                .set_offset_index_disabled(false)
                .set_max_row_group_row_count(None)
                .set_sorting_columns(Some(sorting));
            let dict_cols = match table {
                TableKindBench::Spans => SPANS_DICTIONARY_COLUMNS,
                TableKindBench::Scores => SCORES_DICTIONARY_COLUMNS,
            };
            for c in descriptor.columns() {
                if let Some(top) = c.path().parts().first().map(String::as_str)
                    && dict_cols.contains(&top)
                {
                    b = b.set_column_dictionary_enabled(c.path().clone(), true);
                }
            }
            let ndv = rows.clamp(1, 100_000) as u64;
            for name in bloom_cols {
                if let Some(p) = find_leaf_path(&descriptor, name) {
                    b = b
                        .set_column_bloom_filter_enabled(p.clone(), true)
                        .set_column_bloom_filter_fpp(p.clone(), 0.01)
                        .set_column_bloom_filter_max_ndv(p, ndv);
                }
            }
            Ok(b.build())
        }
    }
}

fn find_leaf_index(descriptor: &SchemaDescriptor, name: &str) -> Option<i32> {
    descriptor
        .columns()
        .iter()
        .enumerate()
        .find(|(_, c)| c.path().parts().len() == 1 && c.path().parts()[0] == name)
        .and_then(|(i, _)| i32::try_from(i).ok())
}

fn find_leaf_path(descriptor: &SchemaDescriptor, name: &str) -> Option<ColumnPath> {
    find_leaf_index(descriptor, name).map(|i| descriptor.columns()[i as usize].path().clone())
}

pub(crate) fn spans_to_arrow(rows: &[SpanRow]) -> BenchResult<RecordBatch> {
    let n = rows.len();
    let mut project_id = StringBuilder::with_capacity(n, n * 36);
    let mut trace_id = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut span_id = FixedSizeBinaryBuilder::with_capacity(n, 8);
    let mut parent_span_id = FixedSizeBinaryBuilder::with_capacity(n, 8);
    let mut name = StringBuilder::with_capacity(n, n * 16);
    let mut span_kind = StringBuilder::with_capacity(n, n * 8);
    let mut start_time = TimestampNanosecondBuilder::with_capacity(n).with_timezone("UTC");
    let mut end_time = TimestampNanosecondBuilder::with_capacity(n).with_timezone("UTC");
    let mut duration_ns = Int64Builder::with_capacity(n);
    let mut status = StringBuilder::with_capacity(n, n * 6);
    let mut status_message = StringBuilder::with_capacity(n, 0);
    let mut service_name = StringBuilder::with_capacity(n, n * 12);
    let mut session_id = StringBuilder::with_capacity(n, n * 14);
    let mut user_id = StringBuilder::with_capacity(n, n * 10);
    let mut environment = StringBuilder::with_capacity(n, n * 10);
    let mut release = StringBuilder::with_capacity(n, n * 8);
    let mut tags = ListBuilder::new(StringBuilder::new()).with_field(ArrowField::new(
        "item",
        ArrowType::Utf8,
        false,
    ));
    let mut metadata = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new())
        .with_values_field(ArrowField::new("values", ArrowType::Utf8, true));
    let mut provider = StringBuilder::with_capacity(n, n * 10);
    let mut request_model = StringBuilder::with_capacity(n, n * 16);
    let mut response_model = StringBuilder::with_capacity(n, n * 20);
    let mut model = StringBuilder::with_capacity(n, n * 16);
    let mut prompt_name = StringBuilder::with_capacity(n, 0);
    let mut prompt_version = Int64Builder::with_capacity(n);
    let mut operation = StringBuilder::with_capacity(n, n * 6);
    let mut input_tokens = Int64Builder::with_capacity(n);
    let mut output_tokens = Int64Builder::with_capacity(n);
    let mut cache_read_tokens = Int64Builder::with_capacity(n);
    let mut cache_write_tokens = Int64Builder::with_capacity(n);
    let mut reasoning_tokens = Int64Builder::with_capacity(n);
    let mut total_tokens = Int64Builder::with_capacity(n);
    let mut cost_input = Float64Builder::with_capacity(n);
    let mut cost_output = Float64Builder::with_capacity(n);
    let mut cost_total = Float64Builder::with_capacity(n);
    let mut cost_source = StringBuilder::with_capacity(n, n * 8);
    let mut ttft_ns = Int64Builder::with_capacity(n);
    let mut request_params = StringBuilder::with_capacity(n, 0);
    let mut input = StringBuilder::with_capacity(n, 0);
    let mut input_format = StringBuilder::with_capacity(n, n * 8);
    let mut output = StringBuilder::with_capacity(n, 0);
    let mut output_format = StringBuilder::with_capacity(n, n * 8);
    let mut input_preview = StringBuilder::with_capacity(n, 0);
    let mut output_preview = StringBuilder::with_capacity(n, 0);
    let mut system_instructions = StringBuilder::with_capacity(n, 0);
    let mut tool_definitions = StringBuilder::with_capacity(n, 0);
    let mut attributes = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new())
        .with_values_field(ArrowField::new("values", ArrowType::Utf8, true));
    let mut resource_attributes = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new())
        .with_values_field(ArrowField::new("values", ArrowType::Utf8, true));
    let mut scope_name = StringBuilder::with_capacity(n, n * 24);
    let mut scope_version = StringBuilder::with_capacity(n, n * 8);
    let mut events = ListBuilder::new(StructBuilder::new(
        arrow_event_fields(),
        vec![
            Box::new(TimestampNanosecondBuilder::with_capacity(0).with_timezone("UTC")),
            Box::new(StringBuilder::new()),
            Box::new(StringBuilder::new()),
        ],
    ))
    .with_field(ArrowField::new(
        "item",
        ArrowType::Struct(arrow_event_fields()),
        false,
    ));
    let mut links = ListBuilder::new(StructBuilder::new(
        arrow_link_fields(),
        vec![
            Box::new(FixedSizeBinaryBuilder::with_capacity(0, 16)),
            Box::new(FixedSizeBinaryBuilder::with_capacity(0, 8)),
            Box::new(StringBuilder::new()),
        ],
    ))
    .with_field(ArrowField::new(
        "item",
        ArrowType::Struct(arrow_link_fields()),
        false,
    ));
    let mut media_refs = ListBuilder::new(StringBuilder::new()).with_field(ArrowField::new(
        "item",
        ArrowType::Utf8,
        false,
    ));
    let mut truncated = ListBuilder::new(StringBuilder::new()).with_field(ArrowField::new(
        "item",
        ArrowType::Utf8,
        false,
    ));
    let mut conventions = ListBuilder::new(StringBuilder::new()).with_field(ArrowField::new(
        "item",
        ArrowType::Utf8,
        false,
    ));
    let mut ingest_time = TimestampNanosecondBuilder::with_capacity(n).with_timezone("UTC");
    let mut schema_version = UInt16Builder::with_capacity(n);

    for r in rows {
        project_id.append_value(&r.project_id);
        trace_id.append_value(r.trace_id)?;
        span_id.append_value(r.span_id)?;
        match r.parent_span_id {
            Some(p) => parent_span_id.append_value(p)?,
            None => parent_span_id.append_null(),
        }
        name.append_value(&r.name);
        span_kind.append_value(&r.span_kind);
        start_time.append_value(r.start_time);
        end_time.append_value(r.end_time);
        duration_ns.append_value(r.duration_ns);
        status.append_value(&r.status);
        status_message.append_option(r.status_message.as_deref());
        service_name.append_option(r.service_name.as_deref());
        session_id.append_option(r.session_id.as_deref());
        user_id.append_option(r.user_id.as_deref());
        environment.append_option(r.environment.as_deref());
        release.append_option(r.release.as_deref());
        for t in &r.tags {
            tags.values().append_value(t);
        }
        tags.append(true);
        append_arrow_map(&mut metadata, &r.metadata)?;
        provider.append_option(r.provider.as_deref());
        request_model.append_option(r.request_model.as_deref());
        response_model.append_option(r.response_model.as_deref());
        model.append_option(r.model.as_deref());
        prompt_name.append_option(r.prompt_name.as_deref());
        prompt_version.append_option(r.prompt_version);
        operation.append_option(r.operation.as_deref());
        input_tokens.append_option(r.input_tokens);
        output_tokens.append_option(r.output_tokens);
        cache_read_tokens.append_option(r.cache_read_tokens);
        cache_write_tokens.append_option(r.cache_write_tokens);
        reasoning_tokens.append_option(r.reasoning_tokens);
        total_tokens.append_option(r.total_tokens);
        cost_input.append_option(r.cost_input);
        cost_output.append_option(r.cost_output);
        cost_total.append_option(r.cost_total);
        cost_source.append_option(r.cost_source.as_deref());
        ttft_ns.append_option(r.ttft_ns);
        request_params.append_option(r.request_params.as_deref());
        input.append_option(r.input.as_deref());
        input_format.append_option(r.input_format.as_deref());
        output.append_option(r.output.as_deref());
        output_format.append_option(r.output_format.as_deref());
        input_preview.append_option(r.input_preview.as_deref());
        output_preview.append_option(r.output_preview.as_deref());
        system_instructions.append_option(r.system_instructions.as_deref());
        tool_definitions.append_option(r.tool_definitions.as_deref());
        append_arrow_map(&mut attributes, &r.attributes)?;
        append_arrow_map(&mut resource_attributes, &r.resource_attributes)?;
        scope_name.append_option(r.scope_name.as_deref());
        scope_version.append_option(r.scope_version.as_deref());
        {
            let sb = events.values();
            for ev in &r.events {
                if let Some(tb) = sb.field_builder::<TimestampNanosecondBuilder>(0) {
                    tb.append_value(ev.time);
                }
                if let Some(nb) = sb.field_builder::<StringBuilder>(1) {
                    nb.append_value(&ev.name);
                }
                if let Some(ab) = sb.field_builder::<StringBuilder>(2) {
                    ab.append_option(ev.attributes.as_deref());
                }
                sb.append(true);
            }
            events.append(true);
        }
        {
            let sb = links.values();
            for lk in &r.links {
                if let Some(tb) = sb.field_builder::<FixedSizeBinaryBuilder>(0) {
                    tb.append_value(lk.trace_id)?;
                }
                if let Some(spb) = sb.field_builder::<FixedSizeBinaryBuilder>(1) {
                    spb.append_value(lk.span_id)?;
                }
                if let Some(ab) = sb.field_builder::<StringBuilder>(2) {
                    ab.append_option(lk.attributes.as_deref());
                }
                sb.append(true);
            }
            links.append(true);
        }
        for s in &r.media_refs {
            media_refs.values().append_value(s);
        }
        media_refs.append(true);
        for s in &r.truncated {
            truncated.values().append_value(s);
        }
        truncated.append(true);
        for s in &r.conventions {
            conventions.values().append_value(s);
        }
        conventions.append(true);
        ingest_time.append_value(r.ingest_time);
        schema_version.append_value(r.schema_version);
    }

    let cols: Vec<ArrayRef> = vec![
        Arc::new(project_id.finish()),
        Arc::new(trace_id.finish()),
        Arc::new(span_id.finish()),
        Arc::new(parent_span_id.finish()),
        Arc::new(name.finish()),
        Arc::new(span_kind.finish()),
        Arc::new(start_time.finish()),
        Arc::new(end_time.finish()),
        Arc::new(duration_ns.finish()),
        Arc::new(status.finish()),
        Arc::new(status_message.finish()),
        Arc::new(service_name.finish()),
        Arc::new(session_id.finish()),
        Arc::new(user_id.finish()),
        Arc::new(environment.finish()),
        Arc::new(release.finish()),
        Arc::new(tags.finish()),
        Arc::new(metadata.finish()),
        Arc::new(provider.finish()),
        Arc::new(request_model.finish()),
        Arc::new(response_model.finish()),
        Arc::new(model.finish()),
        Arc::new(prompt_name.finish()),
        Arc::new(prompt_version.finish()),
        Arc::new(operation.finish()),
        Arc::new(input_tokens.finish()),
        Arc::new(output_tokens.finish()),
        Arc::new(cache_read_tokens.finish()),
        Arc::new(cache_write_tokens.finish()),
        Arc::new(reasoning_tokens.finish()),
        Arc::new(total_tokens.finish()),
        Arc::new(cost_input.finish()),
        Arc::new(cost_output.finish()),
        Arc::new(cost_total.finish()),
        Arc::new(cost_source.finish()),
        Arc::new(ttft_ns.finish()),
        Arc::new(request_params.finish()),
        Arc::new(input.finish()),
        Arc::new(input_format.finish()),
        Arc::new(output.finish()),
        Arc::new(output_format.finish()),
        Arc::new(input_preview.finish()),
        Arc::new(output_preview.finish()),
        Arc::new(system_instructions.finish()),
        Arc::new(tool_definitions.finish()),
        Arc::new(attributes.finish()),
        Arc::new(resource_attributes.finish()),
        Arc::new(scope_name.finish()),
        Arc::new(scope_version.finish()),
        Arc::new(events.finish()),
        Arc::new(links.finish()),
        Arc::new(media_refs.finish()),
        Arc::new(truncated.finish()),
        Arc::new(conventions.finish()),
        Arc::new(ingest_time.finish()),
        Arc::new(schema_version.finish()),
    ];
    Ok(RecordBatch::try_new(arrow_spans_schema(), cols)?)
}

pub(crate) fn scores_to_arrow(rows: &[ScoreRow]) -> BenchResult<RecordBatch> {
    let n = rows.len();
    let mut project_id = StringBuilder::with_capacity(n, n * 36);
    let mut score_id = StringBuilder::with_capacity(n, n * 36);
    let mut target = StringBuilder::with_capacity(n, n * 8);
    let mut trace_id = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut span_id = FixedSizeBinaryBuilder::with_capacity(n, 8);
    let mut session_id = StringBuilder::with_capacity(n, n * 12);
    let mut user_id = StringBuilder::with_capacity(n, n * 10);
    let mut name = StringBuilder::with_capacity(n, n * 14);
    let mut data_type = StringBuilder::with_capacity(n, n * 10);
    let mut value_num = Float64Builder::with_capacity(n);
    let mut value_str = StringBuilder::with_capacity(n, n * 8);
    let mut reason = StringBuilder::with_capacity(n, n * 16);
    let mut source = StringBuilder::with_capacity(n, n * 6);
    let mut evaluator_id = StringBuilder::with_capacity(n, n * 12);
    let mut rule_id = StringBuilder::with_capacity(n, n * 12);
    let mut experiment_id = StringBuilder::with_capacity(n, n * 12);
    let mut dataset_item_id = StringBuilder::with_capacity(n, n * 12);
    let mut queue_id = StringBuilder::with_capacity(n, n * 12);
    let mut author_user_id = StringBuilder::with_capacity(n, n * 12);
    let mut metadata = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new())
        .with_values_field(ArrowField::new("values", ArrowType::Utf8, true));
    let mut created_at = TimestampNanosecondBuilder::with_capacity(n).with_timezone("UTC");
    let mut updated_at = TimestampNanosecondBuilder::with_capacity(n).with_timezone("UTC");
    let mut deleted = BooleanBuilder::with_capacity(n);
    let mut ingest_time = TimestampNanosecondBuilder::with_capacity(n).with_timezone("UTC");

    for r in rows {
        project_id.append_value(&r.project_id);
        score_id.append_value(&r.score_id);
        target.append_value(&r.target);
        match r.trace_id {
            Some(t) => trace_id.append_value(t)?,
            None => trace_id.append_null(),
        }
        match r.span_id {
            Some(s) => span_id.append_value(s)?,
            None => span_id.append_null(),
        }
        session_id.append_option(r.session_id.as_deref());
        user_id.append_option(r.user_id.as_deref());
        name.append_value(&r.name);
        data_type.append_value(&r.data_type);
        value_num.append_option(r.value_num);
        value_str.append_option(r.value_str.as_deref());
        reason.append_option(r.reason.as_deref());
        source.append_value(&r.source);
        evaluator_id.append_option(r.evaluator_id.as_deref());
        rule_id.append_option(r.rule_id.as_deref());
        experiment_id.append_option(r.experiment_id.as_deref());
        dataset_item_id.append_option(r.dataset_item_id.as_deref());
        queue_id.append_option(r.queue_id.as_deref());
        author_user_id.append_option(r.author_user_id.as_deref());
        append_arrow_map(&mut metadata, &r.metadata)?;
        created_at.append_value(r.created_at);
        updated_at.append_value(r.updated_at);
        deleted.append_value(r.deleted);
        ingest_time.append_value(r.ingest_time);
    }

    let cols: Vec<ArrayRef> = vec![
        Arc::new(project_id.finish()),
        Arc::new(score_id.finish()),
        Arc::new(target.finish()),
        Arc::new(trace_id.finish()),
        Arc::new(span_id.finish()),
        Arc::new(session_id.finish()),
        Arc::new(user_id.finish()),
        Arc::new(name.finish()),
        Arc::new(data_type.finish()),
        Arc::new(value_num.finish()),
        Arc::new(value_str.finish()),
        Arc::new(reason.finish()),
        Arc::new(source.finish()),
        Arc::new(evaluator_id.finish()),
        Arc::new(rule_id.finish()),
        Arc::new(experiment_id.finish()),
        Arc::new(dataset_item_id.finish()),
        Arc::new(queue_id.finish()),
        Arc::new(author_user_id.finish()),
        Arc::new(metadata.finish()),
        Arc::new(created_at.finish()),
        Arc::new(updated_at.finish()),
        Arc::new(deleted.finish()),
        Arc::new(ingest_time.finish()),
    ];
    Ok(RecordBatch::try_new(arrow_scores_schema(), cols)?)
}

fn append_arrow_map(
    builder: &mut MapBuilder<StringBuilder, StringBuilder>,
    map: &BTreeMap<String, Option<String>>,
) -> BenchResult<()> {
    for (k, v) in map {
        builder.keys().append_value(k);
        builder.values().append_option(v.as_deref());
    }
    builder.append(true)?;
    Ok(())
}

pub(crate) fn spans_to_cetra_batch(rows: &[SpanRow]) -> BenchResult<Batch> {
    let n = rows.len();
    let cols = vec![
        col_utf8(rows.iter().map(|r| r.project_id.as_str()))?,
        col_fb16(rows.iter().map(|r| r.trace_id))?,
        col_fb8(rows.iter().map(|r| r.span_id))?,
        col_opt_fb8(rows.iter().map(|r| r.parent_span_id))?,
        col_utf8(rows.iter().map(|r| r.name.as_str()))?,
        col_utf8(rows.iter().map(|r| r.span_kind.as_str()))?,
        col_ts(rows.iter().map(|r| r.start_time))?,
        col_ts(rows.iter().map(|r| r.end_time))?,
        col_i64(rows.iter().map(|r| r.duration_ns))?,
        col_utf8(rows.iter().map(|r| r.status.as_str()))?,
        col_opt_utf8(rows.iter().map(|r| r.status_message.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.service_name.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.session_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.user_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.environment.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.release.as_deref()))?,
        col_utf8_list(rows.iter().map(|r| r.tags.as_slice()))?,
        col_map_utf8(rows.iter().map(|r| &r.metadata))?,
        col_opt_utf8(rows.iter().map(|r| r.provider.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.request_model.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.response_model.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.model.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.prompt_name.as_deref()))?,
        col_opt_i64(rows.iter().map(|r| r.prompt_version))?,
        col_opt_utf8(rows.iter().map(|r| r.operation.as_deref()))?,
        col_opt_i64(rows.iter().map(|r| r.input_tokens))?,
        col_opt_i64(rows.iter().map(|r| r.output_tokens))?,
        col_opt_i64(rows.iter().map(|r| r.cache_read_tokens))?,
        col_opt_i64(rows.iter().map(|r| r.cache_write_tokens))?,
        col_opt_i64(rows.iter().map(|r| r.reasoning_tokens))?,
        col_opt_i64(rows.iter().map(|r| r.total_tokens))?,
        col_opt_f64(rows.iter().map(|r| r.cost_input))?,
        col_opt_f64(rows.iter().map(|r| r.cost_output))?,
        col_opt_f64(rows.iter().map(|r| r.cost_total))?,
        col_opt_utf8(rows.iter().map(|r| r.cost_source.as_deref()))?,
        col_opt_i64(rows.iter().map(|r| r.ttft_ns))?,
        col_opt_utf8(rows.iter().map(|r| r.request_params.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.input.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.input_format.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.output.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.output_format.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.input_preview.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.output_preview.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.system_instructions.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.tool_definitions.as_deref()))?,
        col_map_utf8(rows.iter().map(|r| &r.attributes))?,
        col_map_utf8(rows.iter().map(|r| &r.resource_attributes))?,
        col_opt_utf8(rows.iter().map(|r| r.scope_name.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.scope_version.as_deref()))?,
        col_events(rows.iter().map(|r| r.events.as_slice()))?,
        col_links(rows.iter().map(|r| r.links.as_slice()))?,
        col_utf8_list(rows.iter().map(|r| r.media_refs.as_slice()))?,
        col_utf8_list(rows.iter().map(|r| r.truncated.as_slice()))?,
        col_utf8_list(rows.iter().map(|r| r.conventions.as_slice()))?,
        col_ts(rows.iter().map(|r| r.ingest_time))?,
        col_i64(rows.iter().map(|r| i64::from(r.schema_version)))?,
    ];
    Ok(Batch::try_new_all(col_spans_schema(), cols, n)?)
}

pub(crate) fn scores_to_cetra_batch(rows: &[ScoreRow]) -> BenchResult<Batch> {
    let n = rows.len();
    let cols = vec![
        col_utf8(rows.iter().map(|r| r.project_id.as_str()))?,
        col_utf8(rows.iter().map(|r| r.score_id.as_str()))?,
        col_utf8(rows.iter().map(|r| r.target.as_str()))?,
        col_opt_fb16(rows.iter().map(|r| r.trace_id))?,
        col_opt_fb8(rows.iter().map(|r| r.span_id))?,
        col_opt_utf8(rows.iter().map(|r| r.session_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.user_id.as_deref()))?,
        col_utf8(rows.iter().map(|r| r.name.as_str()))?,
        col_utf8(rows.iter().map(|r| r.data_type.as_str()))?,
        col_opt_f64(rows.iter().map(|r| r.value_num))?,
        col_opt_utf8(rows.iter().map(|r| r.value_str.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.reason.as_deref()))?,
        col_utf8(rows.iter().map(|r| r.source.as_str()))?,
        col_opt_utf8(rows.iter().map(|r| r.evaluator_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.rule_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.experiment_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.dataset_item_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.queue_id.as_deref()))?,
        col_opt_utf8(rows.iter().map(|r| r.author_user_id.as_deref()))?,
        col_map_utf8(rows.iter().map(|r| &r.metadata))?,
        col_ts(rows.iter().map(|r| r.created_at))?,
        col_ts(rows.iter().map(|r| r.updated_at))?,
        col_bool(rows.iter().map(|r| r.deleted))?,
        col_ts(rows.iter().map(|r| r.ingest_time))?,
    ];
    Ok(Batch::try_new_all(col_scores_schema(), cols, n)?)
}

fn col_utf8<'a>(iter: impl Iterator<Item = &'a str>) -> BenchResult<Column> {
    let data = Utf8Data::try_from_slices(iter)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(data),
        None,
    )?))
}

fn col_opt_utf8<'a>(iter: impl Iterator<Item = Option<&'a str>>) -> BenchResult<Column> {
    let items: Vec<Option<&'a str>> = iter.collect();
    let mut valid = Vec::with_capacity(items.len());
    let mut any_null = false;
    for opt in &items {
        valid.push(opt.is_some());
        if opt.is_none() {
            any_null = true;
        }
    }
    let data = Utf8Data::try_from_slices(items.into_iter().map(|o| o.unwrap_or("")))?;
    let nulls = any_null.then(|| Bitmap::from_bools(&valid));
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(data),
        nulls,
    )?))
}

fn col_fb16(iter: impl Iterator<Item = [u8; 16]>) -> BenchResult<Column> {
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::FixedBytes16(iter.collect()),
        None,
    )?))
}

fn col_opt_fb16(iter: impl Iterator<Item = Option<[u8; 16]>>) -> BenchResult<Column> {
    let items: Vec<Option<[u8; 16]>> = iter.collect();
    let mut vals = Vec::with_capacity(items.len());
    let mut valid = Vec::with_capacity(items.len());
    let mut any_null = false;
    for opt in items {
        valid.push(opt.is_some());
        if opt.is_none() {
            any_null = true;
        }
        vals.push(opt.unwrap_or([0u8; 16]));
    }
    let nulls = any_null.then(|| Bitmap::from_bools(&valid));
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::FixedBytes16(vals),
        nulls,
    )?))
}

fn col_fb8(iter: impl Iterator<Item = [u8; 8]>) -> BenchResult<Column> {
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::FixedBytes8(iter.collect()),
        None,
    )?))
}

fn col_opt_fb8(iter: impl Iterator<Item = Option<[u8; 8]>>) -> BenchResult<Column> {
    let items: Vec<Option<[u8; 8]>> = iter.collect();
    let mut vals = Vec::with_capacity(items.len());
    let mut valid = Vec::with_capacity(items.len());
    let mut any_null = false;
    for opt in items {
        valid.push(opt.is_some());
        if opt.is_none() {
            any_null = true;
        }
        vals.push(opt.unwrap_or([0u8; 8]));
    }
    let nulls = any_null.then(|| Bitmap::from_bools(&valid));
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::FixedBytes8(vals),
        nulls,
    )?))
}

fn col_ts(iter: impl Iterator<Item = i64>) -> BenchResult<Column> {
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs {
            values: iter.collect(),
            utc: true,
        },
        None,
    )?))
}

fn col_i64(iter: impl Iterator<Item = i64>) -> BenchResult<Column> {
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::I64(iter.collect()),
        None,
    )?))
}

fn col_opt_i64(iter: impl Iterator<Item = Option<i64>>) -> BenchResult<Column> {
    let items: Vec<Option<i64>> = iter.collect();
    let mut vals = Vec::with_capacity(items.len());
    let mut valid = Vec::with_capacity(items.len());
    let mut any_null = false;
    for opt in items {
        valid.push(opt.is_some());
        if opt.is_none() {
            any_null = true;
        }
        vals.push(opt.unwrap_or(0));
    }
    let nulls = any_null.then(|| Bitmap::from_bools(&valid));
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::I64(vals),
        nulls,
    )?))
}

fn col_opt_f64(iter: impl Iterator<Item = Option<f64>>) -> BenchResult<Column> {
    let items: Vec<Option<f64>> = iter.collect();
    let mut vals = Vec::with_capacity(items.len());
    let mut valid = Vec::with_capacity(items.len());
    let mut any_null = false;
    for opt in items {
        valid.push(opt.is_some());
        if opt.is_none() {
            any_null = true;
        }
        vals.push(opt.unwrap_or(0.0));
    }
    let nulls = any_null.then(|| Bitmap::from_bools(&valid));
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::F64(vals),
        nulls,
    )?))
}

fn col_bool(iter: impl Iterator<Item = bool>) -> BenchResult<Column> {
    let bools: Vec<bool> = iter.collect();
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Bool(Bitmap::from_bools(&bools)),
        None,
    )?))
}

fn col_utf8_list<'a>(iter: impl Iterator<Item = &'a [String]>) -> BenchResult<Column> {
    let mut offsets = vec![0u32];
    let mut items: Vec<&'a str> = Vec::new();
    for slice in iter {
        for s in slice {
            items.push(s.as_str());
        }
        offsets.push(u32::try_from(items.len())?);
    }
    let child = Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices(items)?),
        None,
    )?);
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: ColType::Utf8,
            offsets,
            values: Box::new(child),
        },
        None,
    )?))
}

fn col_map_utf8<'a>(
    iter: impl Iterator<Item = &'a BTreeMap<String, Option<String>>>,
) -> BenchResult<Column> {
    let mut offsets = vec![0u32];
    let mut keys: Vec<&'a str> = Vec::new();
    let mut vals: Vec<&'a str> = Vec::new();
    let mut valid: Vec<bool> = Vec::new();
    let mut any_null = false;
    for m in iter {
        for (k, v) in m {
            keys.push(k.as_str());
            match v {
                Some(s) => {
                    vals.push(s.as_str());
                    valid.push(true);
                }
                None => {
                    vals.push("");
                    valid.push(false);
                    any_null = true;
                }
            }
        }
        offsets.push(u32::try_from(keys.len())?);
    }
    let kd = Utf8Data::try_from_slices(keys)?;
    let vd = Utf8Data::try_from_slices(vals)?;
    let vnulls = any_null.then(|| Bitmap::from_bools(&valid));
    let md = MapData::try_new(offsets, kd, vd, vnulls)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(md)),
        None,
    )?))
}

fn col_events<'a>(iter: impl Iterator<Item = &'a [SpanEventRow]>) -> BenchResult<Column> {
    let mut offsets = vec![0u32];
    let mut times = Vec::new();
    let mut names: Vec<&'a str> = Vec::new();
    let mut attrs: Vec<&'a str> = Vec::new();
    let mut valid = Vec::new();
    let mut any_null = false;
    for evs in iter {
        for ev in evs {
            times.push(ev.time);
            names.push(ev.name.as_str());
            match &ev.attributes {
                Some(a) => {
                    attrs.push(a.as_str());
                    valid.push(true);
                }
                None => {
                    attrs.push("");
                    valid.push(false);
                    any_null = true;
                }
            }
        }
        offsets.push(u32::try_from(times.len())?);
    }
    let ed = EventsData::try_new(
        offsets,
        times,
        Utf8Data::try_from_slices(names)?,
        Utf8Data::try_from_slices(attrs)?,
        any_null.then(|| Bitmap::from_bools(&valid)),
    )?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Events(Box::new(ed)),
        None,
    )?))
}

fn col_links<'a>(iter: impl Iterator<Item = &'a [SpanLinkRow]>) -> BenchResult<Column> {
    let mut offsets = vec![0u32];
    let mut tids = Vec::new();
    let mut sids = Vec::new();
    let mut attrs: Vec<&'a str> = Vec::new();
    let mut valid = Vec::new();
    let mut any_null = false;
    for lks in iter {
        for lk in lks {
            tids.push(lk.trace_id);
            sids.push(lk.span_id);
            match &lk.attributes {
                Some(a) => {
                    attrs.push(a.as_str());
                    valid.push(true);
                }
                None => {
                    attrs.push("");
                    valid.push(false);
                    any_null = true;
                }
            }
        }
        offsets.push(u32::try_from(tids.len())?);
    }
    let ld = LinksData::try_new(
        offsets,
        tids,
        sids,
        Utf8Data::try_from_slices(attrs)?,
        any_null.then(|| Bitmap::from_bools(&valid)),
    )?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Links(Box::new(ld)),
        None,
    )?))
}

#[allow(dead_code)]
fn _unused_types(_: EventItem, _: LinkItem, _: BinaryData) {}
