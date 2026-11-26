//! Shape B SQL view schemas, table catalog (`SqlCatalog`), and curated `function_catalog`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, LazyLock};

use cetra_column::{DataType, Field, Schema};

use crate::error::QueryError;
use crate::exec::source::{BatchSource, ScanSource};

/// Inclusive `from`, exclusive `to` UTC nanoseconds window applied at base view scans (D101).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    from: i64,
    to: i64,
}

impl TimeWindow {
    /// Builds a window (`from < to`), returning `QueryError::InvalidRequest` when `from >= to`.
    pub fn new(from: i64, to: i64) -> Result<Self, QueryError> {
        if from < to {
            Ok(Self { from, to })
        } else {
            Err(QueryError::invalid_request("from must be before to"))
        }
    }

    /// Inclusive start timestamp (UTC nanoseconds).
    pub fn from(self) -> i64 {
        self.from
    }

    /// Exclusive end timestamp (UTC nanoseconds).
    pub fn to(self) -> i64 {
        self.to
    }
}

/// Kind of view registered in [`SqlCatalog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlViewKind {
    /// Base `spans` view over raw `spans` storage.
    Spans,
    /// Base `traces` view over raw `traces` storage.
    Traces,
    /// Base `scores` view over raw `scores` storage.
    Scores,
    /// Derived `sessions` view aggregated over `traces`.
    Sessions,
    /// Registered in-memory `BatchSource` table (e.g. `dataset_items`, `experiment_items`).
    Batch,
}

/// Definition of one table or view registered in [`SqlCatalog`].
///
/// Manual `fmt::Debug` omits any attached `BatchSource` row payloads (`AGENTS.md` § Privacy).
#[derive(Clone)]
pub struct SqlTableDef {
    /// Canonical lowercase table name.
    pub name: String,
    /// View kind.
    pub kind: SqlViewKind,
    /// User-facing view schema.
    pub schema: Arc<Schema>,
    /// Underlying raw storage schema for `Spans` / `Traces` / `Scores` (`None` for `Sessions` / `Batch`).
    pub raw_schema: Option<Arc<Schema>>,
    /// In-memory source when `kind == SqlViewKind::Batch`.
    pub batch_source: Option<Arc<BatchSource>>,
}

impl fmt::Debug for SqlTableDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlTableDef")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("columns", &self.schema.len())
            .field("has_raw_schema", &self.raw_schema.is_some())
            .field("has_batch_source", &self.batch_source.is_some())
            .finish()
    }
}

/// Sandboxed Shape B SQL table catalog holding the 4 base views plus registered `BatchSource`s.
#[derive(Clone)]
pub struct SqlCatalog {
    tables: HashMap<String, SqlTableDef>,
    window: Option<TimeWindow>,
    visible_from: Option<i64>,
}

impl fmt::Debug for SqlCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlCatalog")
            .field("table_count", &self.tables.len())
            .field("window", &self.window)
            .field("visible_from", &self.visible_from)
            .finish()
    }
}

impl SqlCatalog {
    /// Creates a catalog pre-populated with `spans`, `traces`, `scores`, and `sessions`.
    pub fn new(window: Option<TimeWindow>, visible_from: Option<i64>) -> Self {
        let mut tables = HashMap::with_capacity(6);
        tables.insert(
            "spans".to_owned(),
            SqlTableDef {
                name: "spans".to_owned(),
                kind: SqlViewKind::Spans,
                schema: spans_view_schema(),
                raw_schema: Some(raw_spans_schema()),
                batch_source: None,
            },
        );
        tables.insert(
            "traces".to_owned(),
            SqlTableDef {
                name: "traces".to_owned(),
                kind: SqlViewKind::Traces,
                schema: traces_view_schema(),
                raw_schema: Some(raw_traces_schema()),
                batch_source: None,
            },
        );
        tables.insert(
            "scores".to_owned(),
            SqlTableDef {
                name: "scores".to_owned(),
                kind: SqlViewKind::Scores,
                schema: scores_view_schema(),
                raw_schema: Some(raw_scores_schema()),
                batch_source: None,
            },
        );
        tables.insert(
            "sessions".to_owned(),
            SqlTableDef {
                name: "sessions".to_owned(),
                kind: SqlViewKind::Sessions,
                schema: sessions_view_schema(),
                raw_schema: None,
                batch_source: None,
            },
        );
        Self {
            tables,
            window,
            visible_from,
        }
    }

    /// Registers an in-memory [`BatchSource`] view (e.g. `dataset_items`, `experiment_items`).
    ///
    /// Rejects duplicate names with [`QueryError::InvalidRequest`] (`"{name} is already registered"`).
    pub fn register_batch_source(
        &mut self,
        name: impl Into<String>,
        source: Arc<BatchSource>,
    ) -> Result<(), QueryError> {
        let key = name.into().to_ascii_lowercase();
        if self.tables.contains_key(&key) {
            return Err(QueryError::invalid_request(format!(
                "{key} is already registered"
            )));
        }
        let schema = source.schema();
        self.tables.insert(
            key.clone(),
            SqlTableDef {
                name: key,
                kind: SqlViewKind::Batch,
                schema,
                raw_schema: None,
                batch_source: Some(source),
            },
        );
        Ok(())
    }

    /// Looks up a registered view by canonical lowercase name.
    pub fn get(&self, name: &str) -> Option<&SqlTableDef> {
        self.tables.get(name)
    }

    /// Returns the set of all registered view names for sandbox validation.
    pub fn registered_names(&self) -> HashSet<String> {
        self.tables.keys().cloned().collect()
    }

    /// Time window enforced at base view scans, if configured.
    pub fn window(&self) -> Option<TimeWindow> {
        self.window
    }

    /// Visibility cutoff (`col >= visible_from`) enforced at base view scans, if configured.
    pub fn visible_from(&self) -> Option<i64> {
        self.visible_from
    }
}

fn f(name: &str, data_type: DataType, nullable: bool) -> Field {
    Field::new(name, data_type, nullable)
}

fn ts() -> DataType {
    DataType::timestamp_utc()
}

fn str_list() -> DataType {
    DataType::List(Box::new(DataType::Utf8))
}

fn schema_arc(fields: Vec<Field>) -> Arc<Schema> {
    // All static schema definitions below have non-empty names and valid DataTypes.
    Schema::try_new_arc(fields)
        .unwrap_or_else(|_| Arc::new(Schema::try_new(vec![]).unwrap_or_else(|_| unreachable!())))
}

static RAW_SPANS_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    schema_arc(vec![
        f("project_id", DataType::Utf8, false),
        f("trace_id", DataType::FixedBytes16, false),
        f("span_id", DataType::FixedBytes8, false),
        f("parent_span_id", DataType::FixedBytes8, true),
        f("name", DataType::Utf8, false),
        f("span_kind", DataType::Utf8, false),
        f("start_time", ts(), false),
        f("end_time", ts(), false),
        f("duration_ns", DataType::I64, false),
        f("status", DataType::Utf8, false),
        f("status_message", DataType::Utf8, true),
        f("service_name", DataType::Utf8, true),
        f("session_id", DataType::Utf8, true),
        f("user_id", DataType::Utf8, true),
        f("environment", DataType::Utf8, true),
        f("release", DataType::Utf8, true),
        f("tags", str_list(), false),
        f("metadata", DataType::MapUtf8, false),
        f("provider", DataType::Utf8, true),
        f("request_model", DataType::Utf8, true),
        f("response_model", DataType::Utf8, true),
        f("model", DataType::Utf8, true),
        f("prompt_name", DataType::Utf8, true),
        f("prompt_version", DataType::I64, true),
        f("operation", DataType::Utf8, true),
        f("input_tokens", DataType::I64, true),
        f("output_tokens", DataType::I64, true),
        f("cache_read_tokens", DataType::I64, true),
        f("cache_write_tokens", DataType::I64, true),
        f("reasoning_tokens", DataType::I64, true),
        f("total_tokens", DataType::I64, true),
        f("cost_input", DataType::F64, true),
        f("cost_output", DataType::F64, true),
        f("cost_total", DataType::F64, true),
        f("cost_source", DataType::Utf8, true),
        f("ttft_ns", DataType::I64, true),
        f("request_params", DataType::Utf8, true),
        f("input", DataType::Utf8, true),
        f("input_format", DataType::Utf8, true),
        f("output", DataType::Utf8, true),
        f("output_format", DataType::Utf8, true),
        f("input_preview", DataType::Utf8, true),
        f("output_preview", DataType::Utf8, true),
        f("system_instructions", DataType::Utf8, true),
        f("tool_definitions", DataType::Utf8, true),
        f("attributes", DataType::MapUtf8, false),
        f("resource_attributes", DataType::MapUtf8, false),
        f("scope_name", DataType::Utf8, true),
        f("scope_version", DataType::Utf8, true),
        f("events", DataType::Events, false),
        f("links", DataType::Links, false),
        f("media_refs", str_list(), false),
        f("truncated", str_list(), false),
        f("conventions", str_list(), false),
        f("ingest_time", ts(), false),
        f("schema_version", DataType::I64, false),
    ])
});

static SPANS_VIEW_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    schema_arc(vec![
        f("trace_id", DataType::Utf8, false),
        f("span_id", DataType::Utf8, false),
        f("parent_span_id", DataType::Utf8, true),
        f("name", DataType::Utf8, false),
        f("span_kind", DataType::Utf8, false),
        f("start_time", ts(), false),
        f("end_time", ts(), false),
        f("duration_ns", DataType::I64, false),
        f("duration_ms", DataType::F64, false),
        f("status", DataType::Utf8, false),
        f("status_message", DataType::Utf8, true),
        f("service_name", DataType::Utf8, true),
        f("session_id", DataType::Utf8, true),
        f("user_id", DataType::Utf8, true),
        f("environment", DataType::Utf8, true),
        f("release", DataType::Utf8, true),
        f("tags", str_list(), false),
        f("metadata", DataType::MapUtf8, false),
        f("provider", DataType::Utf8, true),
        f("request_model", DataType::Utf8, true),
        f("response_model", DataType::Utf8, true),
        f("model", DataType::Utf8, true),
        f("prompt_name", DataType::Utf8, true),
        f("prompt_version", DataType::I64, true),
        f("operation", DataType::Utf8, true),
        f("input_tokens", DataType::I64, true),
        f("output_tokens", DataType::I64, true),
        f("cache_read_tokens", DataType::I64, true),
        f("cache_write_tokens", DataType::I64, true),
        f("reasoning_tokens", DataType::I64, true),
        f("total_tokens", DataType::I64, true),
        f("cost_input", DataType::F64, true),
        f("cost_output", DataType::F64, true),
        f("cost_total", DataType::F64, true),
        f("cost_source", DataType::Utf8, true),
        f("ttft_ns", DataType::I64, true),
        f("request_params", DataType::Utf8, true),
        f("input", DataType::Utf8, true),
        f("input_format", DataType::Utf8, true),
        f("output", DataType::Utf8, true),
        f("output_format", DataType::Utf8, true),
        f("input_preview", DataType::Utf8, true),
        f("output_preview", DataType::Utf8, true),
        f("system_instructions", DataType::Utf8, true),
        f("tool_definitions", DataType::Utf8, true),
        f("attributes", DataType::MapUtf8, false),
        f("resource_attributes", DataType::MapUtf8, false),
        f("scope_name", DataType::Utf8, true),
        f("scope_version", DataType::Utf8, true),
        f("events", DataType::Events, false),
        f("links", DataType::Links, false),
        f("media_refs", str_list(), false),
        f("truncated", str_list(), false),
        f("conventions", str_list(), false),
        f("ingest_time", ts(), false),
        f("schema_version", DataType::I64, false),
    ])
});

static RAW_TRACES_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    schema_arc(vec![
        f("project_id", DataType::Utf8, false),
        f("trace_id", DataType::FixedBytes16, false),
        f("start_time", ts(), false),
        f("end_time", ts(), false),
        f("name", DataType::Utf8, false),
        f("root_span_id", DataType::FixedBytes8, true),
        f("has_root", DataType::Bool, false),
        f("status", DataType::Utf8, false),
        f("session_id", DataType::Utf8, true),
        f("user_id", DataType::Utf8, true),
        f("environment", DataType::Utf8, true),
        f("release", DataType::Utf8, true),
        f("tags", str_list(), false),
        f("metadata", DataType::MapUtf8, false),
        f("input_preview", DataType::Utf8, true),
        f("output_preview", DataType::Utf8, true),
        f("span_count", DataType::I64, false),
        f("error_count", DataType::I64, false),
        f("llm_count", DataType::I64, false),
        f("input_tokens", DataType::I64, true),
        f("output_tokens", DataType::I64, true),
        f("cache_read_tokens", DataType::I64, true),
        f("cache_write_tokens", DataType::I64, true),
        f("reasoning_tokens", DataType::I64, true),
        f("total_tokens", DataType::I64, true),
        f("cost_total", DataType::F64, true),
        f("models", str_list(), false),
        f("prompts", str_list(), false),
        f("last_ingest_time", ts(), false),
        f("partial_count", DataType::I64, false),
    ])
});

static TRACES_VIEW_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    schema_arc(vec![
        f("trace_id", DataType::Utf8, false),
        f("start_time", ts(), false),
        f("end_time", ts(), false),
        f("duration_ns", DataType::I64, false),
        f("duration_ms", DataType::F64, false),
        f("name", DataType::Utf8, false),
        f("root_span_id", DataType::Utf8, true),
        f("has_root", DataType::Bool, false),
        f("status", DataType::Utf8, false),
        f("session_id", DataType::Utf8, true),
        f("user_id", DataType::Utf8, true),
        f("environment", DataType::Utf8, true),
        f("release", DataType::Utf8, true),
        f("tags", str_list(), false),
        f("metadata", DataType::MapUtf8, false),
        f("input_preview", DataType::Utf8, true),
        f("output_preview", DataType::Utf8, true),
        f("span_count", DataType::I64, false),
        f("error_count", DataType::I64, false),
        f("llm_count", DataType::I64, false),
        f("input_tokens", DataType::I64, true),
        f("output_tokens", DataType::I64, true),
        f("cache_read_tokens", DataType::I64, true),
        f("cache_write_tokens", DataType::I64, true),
        f("reasoning_tokens", DataType::I64, true),
        f("total_tokens", DataType::I64, true),
        f("cost_total", DataType::F64, true),
        f("models", str_list(), false),
        f("prompts", str_list(), false),
    ])
});

static RAW_SCORES_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    schema_arc(vec![
        f("project_id", DataType::Utf8, false),
        f("score_id", DataType::Utf8, false),
        f("target", DataType::Utf8, false),
        f("trace_id", DataType::FixedBytes16, true),
        f("span_id", DataType::FixedBytes8, true),
        f("session_id", DataType::Utf8, true),
        f("user_id", DataType::Utf8, true),
        f("name", DataType::Utf8, false),
        f("data_type", DataType::Utf8, false),
        f("value_num", DataType::F64, true),
        f("value_str", DataType::Utf8, true),
        f("reason", DataType::Utf8, true),
        f("source", DataType::Utf8, false),
        f("evaluator_id", DataType::Utf8, true),
        f("rule_id", DataType::Utf8, true),
        f("experiment_id", DataType::Utf8, true),
        f("dataset_item_id", DataType::Utf8, true),
        f("queue_id", DataType::Utf8, true),
        f("author_user_id", DataType::Utf8, true),
        f("metadata", DataType::MapUtf8, false),
        f("created_at", ts(), false),
        f("updated_at", ts(), false),
        f("deleted", DataType::Bool, false),
        f("ingest_time", ts(), false),
    ])
});

static SCORES_VIEW_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    schema_arc(vec![
        f("score_id", DataType::Utf8, false),
        f("target", DataType::Utf8, false),
        f("trace_id", DataType::Utf8, true),
        f("span_id", DataType::Utf8, true),
        f("session_id", DataType::Utf8, true),
        f("user_id", DataType::Utf8, true),
        f("name", DataType::Utf8, false),
        f("data_type", DataType::Utf8, false),
        f("value_num", DataType::F64, true),
        f("value_str", DataType::Utf8, true),
        f("reason", DataType::Utf8, true),
        f("source", DataType::Utf8, false),
        f("evaluator_id", DataType::Utf8, true),
        f("rule_id", DataType::Utf8, true),
        f("experiment_id", DataType::Utf8, true),
        f("dataset_item_id", DataType::Utf8, true),
        f("queue_id", DataType::Utf8, true),
        f("author_user_id", DataType::Utf8, true),
        f("metadata", DataType::MapUtf8, false),
        f("created_at", ts(), false),
        f("updated_at", ts(), false),
        f("deleted", DataType::Bool, false),
        f("ingest_time", ts(), false),
    ])
});

static SESSIONS_VIEW_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    schema_arc(vec![
        f("session_id", DataType::Utf8, false),
        f("start_time", ts(), false),
        f("end_time", ts(), false),
        f("duration_ns", DataType::I64, false),
        f("duration_ms", DataType::F64, false),
        f("trace_count", DataType::I64, false),
        f("error_count", DataType::I64, false),
        f("total_tokens", DataType::I64, true),
        f("cost_total", DataType::F64, true),
        f("user_id", DataType::Utf8, true),
        f("tags", str_list(), false),
    ])
});

/// Raw storage `spans` schema (56 columns, binary IDs, `project_id` included).
pub fn raw_spans_schema() -> Arc<Schema> {
    Arc::clone(&RAW_SPANS_SCHEMA)
}

/// User-facing `spans` view schema (56 columns, hex IDs, `duration_ms`, no `project_id`).
pub fn spans_view_schema() -> Arc<Schema> {
    Arc::clone(&SPANS_VIEW_SCHEMA)
}

/// Raw storage `traces` schema (30 columns, binary IDs, bookkeeping columns included).
pub fn raw_traces_schema() -> Arc<Schema> {
    Arc::clone(&RAW_TRACES_SCHEMA)
}

/// User-facing `traces` view schema (29 columns, hex IDs, `duration_ns` + `duration_ms`).
pub fn traces_view_schema() -> Arc<Schema> {
    Arc::clone(&TRACES_VIEW_SCHEMA)
}

/// Raw storage `scores` schema (24 columns, binary IDs, `project_id` included).
pub fn raw_scores_schema() -> Arc<Schema> {
    Arc::clone(&RAW_SCORES_SCHEMA)
}

/// User-facing `scores` view schema (23 columns, hex IDs, no `project_id`).
pub fn scores_view_schema() -> Arc<Schema> {
    Arc::clone(&SCORES_VIEW_SCHEMA)
}

/// User-facing `sessions` view schema (11 columns).
pub fn sessions_view_schema() -> Arc<Schema> {
    Arc::clone(&SESSIONS_VIEW_SCHEMA)
}

// ---------------------------------------------------------------------------
// Curated Shape B function catalog
// ---------------------------------------------------------------------------

/// Category of a curated function in [`function_catalog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    /// Scalar function.
    Scalar,
    /// Aggregate function.
    Aggregate,
    /// Window function.
    Window,
    /// Filter-only score function.
    FilterScore,
}

/// One function the Filter DSL or Shape B SQL console may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionInfo {
    /// Canonical lowercase function name.
    pub name: &'static str,
    /// Signature shown to clients.
    pub signature: &'static str,
    /// Text served by `GET /schema`.
    pub description: &'static str,
    /// Function category.
    pub kind: FunctionKind,
    /// Usable in the Filter DSL.
    pub in_filter: bool,
    /// Usable in the Shape B SQL console.
    pub in_sql: bool,
}

const fn func(
    name: &'static str,
    signature: &'static str,
    description: &'static str,
    kind: FunctionKind,
    in_filter: bool,
    in_sql: bool,
) -> FunctionInfo {
    FunctionInfo {
        name,
        signature,
        description,
        kind,
        in_filter,
        in_sql,
    }
}

const FUNCTIONS: &[FunctionInfo] = &[
    // --- Scalars (50, matching `ir::ScalarFn`) ---
    func(
        "lower",
        "lower(value: string) -> string",
        "Lowercase a string.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "upper",
        "upper(value: string) -> string",
        "Uppercase a string.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "length",
        "length(value: string) -> integer",
        "Number of UTF-8 characters.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "substr",
        "substr(value: string, start: integer [, count: integer]) -> string",
        "Extract a substring (1-based start).",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "trim",
        "trim(value: string [, chars: string]) -> string",
        "Trim leading and trailing characters.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "ltrim",
        "ltrim(value: string [, chars: string]) -> string",
        "Trim leading characters.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "rtrim",
        "rtrim(value: string [, chars: string]) -> string",
        "Trim trailing characters.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "left",
        "left(value: string, n: integer) -> string",
        "First n characters of a string.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "right",
        "right(value: string, n: integer) -> string",
        "Last n characters of a string.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "split_part",
        "split_part(value: string, delimiter: string, part: integer) -> string",
        "Split a string and return the 1-based part.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "concat",
        "concat(value: any, ...) -> string",
        "Concatenate values as a string.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "concat_ws",
        "concat_ws(separator: string, value: any, ...) -> string",
        "Concatenate non-null values with a separator.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "starts_with",
        "starts_with(value: string, prefix: string) -> boolean",
        "Whether value starts with prefix.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "ends_with",
        "ends_with(value: string, suffix: string) -> boolean",
        "Whether value ends with suffix.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "contains",
        "contains(value: string, substring: string) -> boolean",
        "Whether value contains substring.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "strpos",
        "strpos(value: string, substring: string) -> integer",
        "1-based position of substring, or 0 if absent.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "regexp_like",
        "regexp_like(value: string, pattern: string [, flags: string]) -> boolean",
        "Whether value matches a regular expression.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "regexp_extract",
        "regexp_extract(value: string, pattern: string [, flags: string]) -> string",
        "Extract the first match or capture group.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "encode",
        "encode(value: binary | string, encoding: string) -> string",
        "Encode bytes using hex or base64.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "now",
        "now() -> timestamp",
        "Current query timestamp.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "date_trunc",
        "date_trunc(unit: string, time: timestamp) -> timestamp",
        "Truncate a timestamp to a unit such as day or hour.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "date_bin",
        "date_bin(stride: interval, time: timestamp [, origin: timestamp]) -> timestamp",
        "Bin a timestamp to fixed stride boundaries.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "extract",
        "extract(field: string, value: timestamp | date | interval) -> integer | float",
        "Extract a calendar field from a timestamp, date, or interval.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "to_timestamp",
        "to_timestamp(value: integer | float | string | timestamp) -> timestamp",
        "Convert epoch seconds or string to a UTC timestamp.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "abs",
        "abs(value: number) -> number",
        "Absolute value.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "round",
        "round(value: number [, digits: integer]) -> number",
        "Round a number to digits decimal places.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "floor",
        "floor(value: number) -> number",
        "Largest integer not greater than value.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "ceil",
        "ceil(value: number) -> number",
        "Smallest integer not less than value.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "sqrt",
        "sqrt(value: float) -> float",
        "Square root.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "ln",
        "ln(value: float) -> float",
        "Natural logarithm.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "log10",
        "log10(value: float) -> float",
        "Base-10 logarithm.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "power",
        "power(base: float, exponent: float) -> float",
        "Raise base to exponent.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "coalesce",
        "coalesce(value: any, fallback: any, ...) -> any",
        "First non-null argument.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "nullif",
        "nullif(a: any, b: any) -> any",
        "Return null when a equals b, else a.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "greatest",
        "greatest(value: any, ...) -> any",
        "Largest non-null argument.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "least",
        "least(value: any, ...) -> any",
        "Smallest non-null argument.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "isnan",
        "isnan(value: float) -> boolean",
        "Whether value is IEEE NaN.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "isfinite",
        "isfinite(value: float) -> boolean",
        "Whether value is finite (not NaN or infinite).",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "array_has",
        "array_has(array: list, value: any) -> boolean",
        "Whether the list contains the value.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "array_has_all",
        "array_has_all(array: list, subset: list) -> boolean",
        "Whether array contains every element of subset.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "array_length",
        "array_length(array: list [, dim: integer]) -> integer",
        "Number of elements.",
        FunctionKind::Scalar,
        true,
        true,
    ),
    func(
        "array_element",
        "array_element(array: list, index: integer) -> any",
        "1-based element of a list.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "array_to_string",
        "array_to_string(array: list, delimiter: string [, null_str: string]) -> string",
        "Join list elements into a string.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "array_distinct",
        "array_distinct(array: list) -> list",
        "Remove duplicate elements from a list.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "map_keys",
        "map_keys(map: string_map) -> string_list",
        "Sorted keys of a string map.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "map_values",
        "map_values(map: string_map) -> string_list",
        "Values of a string map in key order.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "json_get",
        "json_get(json: string, key: string | integer) -> string",
        "Get JSON object field or array element as JSON text.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "json_get_text",
        "json_get_text(json: string, key: string | integer) -> string",
        "Get JSON object field or array element as unquoted text.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "json_extract",
        "json_extract(json: string, path: string) -> string",
        "Extract JSONPath value as JSON text.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    func(
        "json_extract_text",
        "json_extract_text(json: string, path: string) -> string",
        "Extract JSONPath value as unquoted text.",
        FunctionKind::Scalar,
        false,
        true,
    ),
    // --- Aggregates (10) ---
    func(
        "count",
        "count([DISTINCT] value: any | *) -> integer",
        "Count rows or non-null values.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "sum",
        "sum(value: number) -> number",
        "Sum of non-null values.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "avg",
        "avg(value: number) -> float | decimal",
        "Arithmetic mean of non-null values.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "min",
        "min(value: any) -> any",
        "Minimum non-null value.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "max",
        "max(value: any) -> any",
        "Maximum non-null value.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "approx_percentile_cont",
        "approx_percentile_cont(value: number, quantile: float) -> float",
        "Approximate continuous percentile via DDSketch.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "percentile_cont",
        "percentile_cont(quantile: float) WITHIN GROUP (ORDER BY value) -> float",
        "Exact interpolated continuous percentile.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "bool_or",
        "bool_or(value: boolean) -> boolean",
        "True if any non-null value is true.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "bool_and",
        "bool_and(value: boolean) -> boolean",
        "True if all non-null values are true.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    func(
        "array_agg",
        "array_agg([DISTINCT] value: any) -> list",
        "Collect non-null values into a list.",
        FunctionKind::Aggregate,
        false,
        true,
    ),
    // --- Window functions (7) ---
    func(
        "row_number",
        "row_number() OVER (...) -> integer",
        "Sequential 1-based row number within each partition.",
        FunctionKind::Window,
        false,
        true,
    ),
    func(
        "rank",
        "rank() OVER (...) -> integer",
        "1-based rank with gaps for ties.",
        FunctionKind::Window,
        false,
        true,
    ),
    func(
        "dense_rank",
        "dense_rank() OVER (...) -> integer",
        "1-based rank without gaps for ties.",
        FunctionKind::Window,
        false,
        true,
    ),
    func(
        "lag",
        "lag(value: any [, offset: integer [, default: any]]) OVER (...) -> any",
        "Value from a preceding row in the partition.",
        FunctionKind::Window,
        false,
        true,
    ),
    func(
        "lead",
        "lead(value: any [, offset: integer [, default: any]]) OVER (...) -> any",
        "Value from a following row in the partition.",
        FunctionKind::Window,
        false,
        true,
    ),
    func(
        "first_value",
        "first_value(value: any) OVER (...) -> any",
        "Value at the first row of the window frame.",
        FunctionKind::Window,
        false,
        true,
    ),
    func(
        "last_value",
        "last_value(value: any) OVER (...) -> any",
        "Value at the last row of the window frame.",
        FunctionKind::Window,
        false,
        true,
    ),
    // --- Filter-only score functions (3, D97) ---
    func(
        "score",
        "score(name: string) -> float",
        "Latest numeric score of that name on the row's target.",
        FunctionKind::FilterScore,
        true,
        false,
    ),
    func(
        "score_label",
        "score_label(name: string) -> string",
        "Latest categorical or text score of that name.",
        FunctionKind::FilterScore,
        true,
        false,
    ),
    func(
        "has_score",
        "has_score(name: string) -> boolean",
        "Whether the row's target has a score of that name.",
        FunctionKind::FilterScore,
        true,
        false,
    ),
];

/// Returns all curated Filter DSL and Shape B SQL functions.
pub fn function_catalog() -> &'static [FunctionInfo] {
    FUNCTIONS
}

#[cfg(test)]
mod tests {
    use cetra_column::Field;

    use super::*;
    use crate::ir::ScalarFn;
    use crate::text::DENIED_FUNCTIONS;

    fn all_scalar_fns() -> Vec<ScalarFn> {
        // Exhaustive list verified via exhaustive match below.
        let sample = ScalarFn::Lower;
        match sample {
            ScalarFn::Lower
            | ScalarFn::Upper
            | ScalarFn::Length
            | ScalarFn::Substr
            | ScalarFn::Trim
            | ScalarFn::Ltrim
            | ScalarFn::Rtrim
            | ScalarFn::Left
            | ScalarFn::Right
            | ScalarFn::SplitPart
            | ScalarFn::Concat
            | ScalarFn::ConcatWs
            | ScalarFn::StartsWith
            | ScalarFn::EndsWith
            | ScalarFn::Contains
            | ScalarFn::Strpos
            | ScalarFn::RegexpLike
            | ScalarFn::RegexpExtract
            | ScalarFn::Encode
            | ScalarFn::Now
            | ScalarFn::DateTrunc
            | ScalarFn::DateBin
            | ScalarFn::Extract
            | ScalarFn::ToTimestamp
            | ScalarFn::Abs
            | ScalarFn::Round
            | ScalarFn::Floor
            | ScalarFn::Ceil
            | ScalarFn::Sqrt
            | ScalarFn::Ln
            | ScalarFn::Log10
            | ScalarFn::Power
            | ScalarFn::Coalesce
            | ScalarFn::Nullif
            | ScalarFn::Greatest
            | ScalarFn::Least
            | ScalarFn::IsNan
            | ScalarFn::IsFinite
            | ScalarFn::ArrayHas
            | ScalarFn::ArrayHasAll
            | ScalarFn::ArrayLength
            | ScalarFn::ArrayElement
            | ScalarFn::ArrayToString
            | ScalarFn::ArrayDistinct
            | ScalarFn::MapKeys
            | ScalarFn::MapValues
            | ScalarFn::JsonGet
            | ScalarFn::JsonGetText
            | ScalarFn::JsonExtract
            | ScalarFn::JsonExtractText => {}
        }
        vec![
            ScalarFn::Lower,
            ScalarFn::Upper,
            ScalarFn::Length,
            ScalarFn::Substr,
            ScalarFn::Trim,
            ScalarFn::Ltrim,
            ScalarFn::Rtrim,
            ScalarFn::Left,
            ScalarFn::Right,
            ScalarFn::SplitPart,
            ScalarFn::Concat,
            ScalarFn::ConcatWs,
            ScalarFn::StartsWith,
            ScalarFn::EndsWith,
            ScalarFn::Contains,
            ScalarFn::Strpos,
            ScalarFn::RegexpLike,
            ScalarFn::RegexpExtract,
            ScalarFn::Encode,
            ScalarFn::Now,
            ScalarFn::DateTrunc,
            ScalarFn::DateBin,
            ScalarFn::Extract,
            ScalarFn::ToTimestamp,
            ScalarFn::Abs,
            ScalarFn::Round,
            ScalarFn::Floor,
            ScalarFn::Ceil,
            ScalarFn::Sqrt,
            ScalarFn::Ln,
            ScalarFn::Log10,
            ScalarFn::Power,
            ScalarFn::Coalesce,
            ScalarFn::Nullif,
            ScalarFn::Greatest,
            ScalarFn::Least,
            ScalarFn::IsNan,
            ScalarFn::IsFinite,
            ScalarFn::ArrayHas,
            ScalarFn::ArrayHasAll,
            ScalarFn::ArrayLength,
            ScalarFn::ArrayElement,
            ScalarFn::ArrayToString,
            ScalarFn::ArrayDistinct,
            ScalarFn::MapKeys,
            ScalarFn::MapValues,
            ScalarFn::JsonGet,
            ScalarFn::JsonGetText,
            ScalarFn::JsonExtract,
            ScalarFn::JsonExtractText,
        ]
    }

    #[test]
    fn function_catalog_covers_all_scalars_and_excludes_denied() {
        let cat = function_catalog();
        let mut seen = HashSet::new();
        for info in cat {
            assert!(seen.insert(info.name), "duplicate function {}", info.name);
            assert!(
                !DENIED_FUNCTIONS.contains(&info.name),
                "denied function {} in catalog",
                info.name
            );
        }

        // Every `ScalarFn` variant is present with `in_sql: true`.
        for sfn in all_scalar_fns() {
            let entry = cat
                .iter()
                .find(|f| f.name == sfn.name())
                .unwrap_or_else(|| panic!("missing ScalarFn {}", sfn.name()));
            assert_eq!(entry.kind, FunctionKind::Scalar);
            assert!(entry.in_sql, "{} must have in_sql = true", entry.name);
        }

        let scalars = cat
            .iter()
            .filter(|f| f.kind == FunctionKind::Scalar)
            .count();
        let aggs = cat
            .iter()
            .filter(|f| f.kind == FunctionKind::Aggregate)
            .count();
        let wins = cat
            .iter()
            .filter(|f| f.kind == FunctionKind::Window)
            .count();
        let scores = cat
            .iter()
            .filter(|f| f.kind == FunctionKind::FilterScore)
            .count();
        assert_eq!(scalars, 50);
        assert_eq!(aggs, 10);
        assert_eq!(wins, 7);
        assert_eq!(scores, 3);
        for f in cat.iter().filter(|f| f.kind == FunctionKind::FilterScore) {
            assert!(f.in_filter);
            assert!(!f.in_sql);
        }
    }

    #[test]
    fn catalog_views_and_batch_source_registration() {
        let win = TimeWindow::new(10, 20).unwrap();
        assert_eq!(win.from(), 10);
        assert_eq!(win.to(), 20);
        assert!(matches!(
            TimeWindow::new(20, 20),
            Err(QueryError::InvalidRequest(_))
        ));
        assert!(matches!(
            TimeWindow::new(21, 20),
            Err(QueryError::InvalidRequest(_))
        ));

        let mut cat = SqlCatalog::new(Some(win), Some(5));
        assert_eq!(cat.get("spans").unwrap().schema.len(), 56);
        assert_eq!(cat.get("traces").unwrap().schema.len(), 29);
        assert_eq!(cat.get("scores").unwrap().schema.len(), 23);
        assert_eq!(cat.get("sessions").unwrap().schema.len(), 11);

        let ds_schema = Schema::try_new_arc(vec![Field::new("id", DataType::Utf8, false)]).unwrap();
        let ds_src = Arc::new(BatchSource::empty(ds_schema));
        cat.register_batch_source("dataset_items", Arc::clone(&ds_src))
            .unwrap();
        assert_eq!(cat.get("dataset_items").unwrap().kind, SqlViewKind::Batch);
        let err = cat
            .register_batch_source("dataset_items", ds_src)
            .unwrap_err();
        assert!(matches!(
            err,
            QueryError::InvalidRequest(ref m) if m == "dataset_items is already registered"
        ));

        let dbg = format!("{cat:?}");
        assert!(dbg.contains("SqlCatalog"));
    }
}
