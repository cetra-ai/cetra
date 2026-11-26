//! The reads that become `expected.json` (R8.6, plan § dump.rs): the public reads of `Storage`
//! and `reads`, with the request values `model_tests/observe.rs` uses, on an explicit window,
//! every page followed. A dump runs on a copy of a directory only.

use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use arrow::array::RecordBatch;
use cetra_column::{
    Batch, Column, EventsData, FlatColumn, FlatData, Interval, LinksData, MapData, ScalarValue,
    Selection,
};
use cetra_core::ids::ProjectId;
use cetra_core::schema::{event_field, link_field, scores, spans, traces};
use chrono::{DateTime, NaiveDate, SecondsFormat};
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::{Connection, Row, SqliteConnection};

use super::{
    CACHE_LOCATION, CACHE_NAME, CACHE_SIZE, DELETION_TRACE, DELETION_USER, GENERATOR, MEDIA, P1,
    P2, P3, ns_at, window,
};
use crate::format::FORMAT_VERSION;
use crate::test_support::headline_kpis;
use crate::{
    DeletionStatus, ListScope, MAX_PAGE, QueryContext, QueryOptions, ScoreListRequest,
    ScoreTargets, SessionListRequest, SessionSort, SortOrder, SpanListRequest, SpanSort, Storage,
    TimeWindow, TraceListRequest, TraceSort, UserListRequest, UserSort, latest_scores, score_list,
    session_list, span_detail, span_list, trace_detail, trace_list, user_list,
};

/// Time allowed for each read request.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The window of a read, as an explicit `TimeWindow` (never the clock).
fn time_window() -> TimeWindow {
    let (from, to) = window();
    TimeWindow::new(from, to).expect("the golden window has from before to")
}

/// The scope of every list read: the golden window, evaluation and experiment rows kept.
fn scope() -> ListScope {
    ListScope {
        window: Some(time_window()),
        include_eval: true,
        include_experiments: true,
    }
}

/// A query context on `project`, as the server's read handlers build one.
async fn context(storage: &Storage, project: ProjectId) -> QueryContext {
    let query = storage
        .query(project, QueryOptions::new(None, None))
        .await
        .unwrap_or_else(|error| panic!("query a golden project: {error}"));
    QueryContext::new(query, Some(time_window())).unwrap_or_else(|error| panic!("context: {error}"))
}

/// The rows of `batches`, projected to `columns` by name, as JSON objects in order.
fn rows(batches: &[RecordBatch], columns: &[&str]) -> Vec<Value> {
    let mut out = Vec::new();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let col_batch = crate::column_bridge::record_batch_to_column(batch)
            .expect("convert a read result to cetra_column::Batch");
        out.extend(batch_rows(&col_batch, columns));
    }
    out
}

/// The rows of `batch`, projected to `columns` by name, as JSON objects in order.
/// Matches `arrow::json::ArrayWriter` (`explicit_nulls = false`) across all encodings and selections.
fn batch_rows(batch: &Batch, columns: &[&str]) -> Vec<Value> {
    if batch.is_empty() {
        return Vec::new();
    }
    let indices: Vec<usize> = columns
        .iter()
        .map(|name| {
            batch
                .schema()
                .index_of(name)
                .unwrap_or_else(|| panic!("the read result has a {name} column"))
        })
        .collect();
    let projected = batch
        .project(&indices)
        .expect("project the columns of a read result");
    let mut out = Vec::with_capacity(projected.num_rows());
    let fields = projected.schema().fields();
    let cols = projected.columns();
    let mut push_row = |phys_row: usize| {
        let mut map = Map::new();
        for (field, col) in fields.iter().zip(cols) {
            if col.is_null(phys_row) {
                continue;
            }
            map.insert(field.name.clone(), cell_to_json(col, phys_row));
        }
        out.push(sorted_keys(Value::Object(map)));
    };
    match projected.selection() {
        Selection::All => {
            for phys_row in 0..projected.raw_len() {
                push_row(phys_row);
            }
        }
        Selection::Mask(mask) => {
            for phys_row in mask.iter_ones() {
                push_row(phys_row);
            }
        }
        Selection::Indices(indices) => {
            for &phys_row in indices {
                push_row(phys_row as usize);
            }
        }
    }
    out
}

fn resolve_flat(col: &Column, phys_row: usize) -> (&FlatColumn, usize) {
    match col {
        Column::Flat(flat) => (flat, phys_row),
        Column::Dict(dict) => (dict.values(), dict.codes()[phys_row] as usize),
        Column::Rle(rle) => {
            let run = rle
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            (rle.values(), run)
        }
        Column::Const { .. } => unreachable!("Const handled by caller"),
    }
}

fn cell_to_json(col: &Column, phys_row: usize) -> Value {
    if col.is_null(phys_row) {
        return Value::Null;
    }
    if let Column::Const { value, .. } = col {
        return scalar_to_json(value);
    }
    let (flat, flat_row) = resolve_flat(col, phys_row);
    flat_to_json(flat, flat_row)
}

fn flat_to_json(flat: &FlatColumn, row: usize) -> Value {
    if flat.is_null(row) {
        return Value::Null;
    }
    match flat.data() {
        FlatData::Null(_) => Value::Null,
        FlatData::Bool(bm) => Value::Bool(bm.get(row)),
        FlatData::I64(vals) => Value::Number(Number::from(vals[row])),
        FlatData::F64(vals) => Number::from_f64(vals[row]).map_or(Value::Null, Value::Number),
        FlatData::Decimal128 { values, scale, .. } => decimal_to_json(values[row], *scale),
        FlatData::TimestampNs { values, utc } => timestamp_to_json(values[row], *utc),
        FlatData::Date(vals) => date_to_json(vals[row]),
        FlatData::Interval(vals) => interval_to_json(vals[row]),
        FlatData::Utf8(utf8) => {
            Value::String(utf8.get(row).expect("utf8 row index in bounds").to_owned())
        }
        FlatData::Binary(bin) => Value::String(hex::encode(
            bin.get(row).expect("binary row index in bounds"),
        )),
        FlatData::FixedBytes8(vals) => Value::String(hex::encode(vals[row])),
        FlatData::FixedBytes16(vals) => Value::String(hex::encode(vals[row])),
        FlatData::List {
            offsets, values, ..
        } => {
            let start = offsets[row] as usize;
            let end = offsets[row + 1] as usize;
            Value::Array((start..end).map(|idx| cell_to_json(values, idx)).collect())
        }
        FlatData::MapUtf8(data) => map_flat_to_json(data, row),
        FlatData::Events(data) => events_flat_to_json(data, row),
        FlatData::Links(data) => links_flat_to_json(data, row),
    }
}

fn scalar_to_json(value: &ScalarValue) -> Value {
    match value {
        ScalarValue::Null(_) => Value::Null,
        ScalarValue::Bool(b) => Value::Bool(*b),
        ScalarValue::I64(n) => Value::Number(Number::from(*n)),
        ScalarValue::F64(x) => Number::from_f64(*x).map_or(Value::Null, Value::Number),
        ScalarValue::Decimal128 { value, scale, .. } => decimal_to_json(*value, *scale),
        ScalarValue::TimestampNs { value, utc } => timestamp_to_json(*value, *utc),
        ScalarValue::Date(days) => date_to_json(*days),
        ScalarValue::Interval(iv) => interval_to_json(*iv),
        ScalarValue::Utf8(s) => Value::String(s.clone()),
        ScalarValue::Binary(b) => Value::String(hex::encode(b)),
        ScalarValue::FixedBytes8(b) => Value::String(hex::encode(b)),
        ScalarValue::FixedBytes16(b) => Value::String(hex::encode(b)),
        ScalarValue::List { values, .. } => {
            Value::Array(values.iter().map(scalar_to_json).collect())
        }
        ScalarValue::MapUtf8(entries) => {
            let mut map = Map::new();
            for (k, v) in entries {
                if let Some(val) = v {
                    map.insert(k.clone(), Value::String(val.clone()));
                }
            }
            Value::Object(map)
        }
        ScalarValue::Events(items) => Value::Array(
            items
                .iter()
                .map(|ev| event_to_json(ev.time, &ev.name, ev.attributes.as_deref()))
                .collect(),
        ),
        ScalarValue::Links(items) => Value::Array(
            items
                .iter()
                .map(|lk| link_to_json(&lk.trace_id, &lk.span_id, lk.attributes.as_deref()))
                .collect(),
        ),
    }
}

fn map_flat_to_json(data: &MapData, row: usize) -> Value {
    let start = data.offsets()[row] as usize;
    let end = data.offsets()[row + 1] as usize;
    let mut map = Map::new();
    for idx in start..end {
        if data.value_nulls().is_some_and(|bm| !bm.get(idx)) {
            continue;
        }
        let key = data.keys().get(idx).expect("map key in bounds");
        let val = data.values().get(idx).expect("map value in bounds");
        map.insert(key.to_owned(), Value::String(val.to_owned()));
    }
    Value::Object(map)
}

fn events_flat_to_json(data: &EventsData, row: usize) -> Value {
    let start = data.offsets()[row] as usize;
    let end = data.offsets()[row + 1] as usize;
    let mut items = Vec::with_capacity(end - start);
    for idx in start..end {
        let time = data.times()[idx];
        let name = data.names().get(idx).expect("event name in bounds");
        let attrs = if data.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
            None
        } else {
            Some(data.attributes().get(idx).expect("event attrs in bounds"))
        };
        items.push(event_to_json(time, name, attrs));
    }
    Value::Array(items)
}

fn event_to_json(time: i64, name: &str, attrs: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert(event_field::TIME.to_owned(), timestamp_to_json(time, true));
    map.insert(event_field::NAME.to_owned(), Value::String(name.to_owned()));
    if let Some(a) = attrs {
        map.insert(
            event_field::ATTRIBUTES.to_owned(),
            Value::String(a.to_owned()),
        );
    }
    Value::Object(map)
}

fn links_flat_to_json(data: &LinksData, row: usize) -> Value {
    let start = data.offsets()[row] as usize;
    let end = data.offsets()[row + 1] as usize;
    let mut items = Vec::with_capacity(end - start);
    for idx in start..end {
        let tid = &data.trace_ids()[idx];
        let sid = &data.span_ids()[idx];
        let attrs = if data.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
            None
        } else {
            Some(data.attributes().get(idx).expect("link attrs in bounds"))
        };
        items.push(link_to_json(tid, sid, attrs));
    }
    Value::Array(items)
}

fn link_to_json(trace_id: &[u8; 16], span_id: &[u8; 8], attrs: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert(
        link_field::TRACE_ID.to_owned(),
        Value::String(hex::encode(trace_id)),
    );
    map.insert(
        link_field::SPAN_ID.to_owned(),
        Value::String(hex::encode(span_id)),
    );
    if let Some(a) = attrs {
        map.insert(
            link_field::ATTRIBUTES.to_owned(),
            Value::String(a.to_owned()),
        );
    }
    Value::Object(map)
}

fn timestamp_to_json(ns: i64, utc: bool) -> Value {
    let dt = DateTime::from_timestamp_nanos(ns);
    if utc {
        Value::String(dt.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    } else {
        Value::String(format!("{:?}", dt.naive_utc()))
    }
}

fn date_to_json(days: i32) -> Value {
    let ce = days
        .checked_add(719_163)
        .expect("date days within NaiveDate range");
    let date = NaiveDate::from_num_days_from_ce_opt(ce).expect("valid NaiveDate");
    Value::String(format!("{date:?}"))
}

fn interval_to_json(iv: Interval) -> Value {
    if iv.months == 0 && iv.days == 0 && iv.nanos == 0 {
        return Value::String("0 secs".to_owned());
    }
    let mut out = String::new();
    let mut prefix = "";
    if iv.months != 0 {
        let _ = write!(out, "{prefix}{} mons", iv.months);
        prefix = " ";
    }
    if iv.days != 0 {
        let _ = write!(out, "{prefix}{} days", iv.days);
        prefix = " ";
    }
    if iv.nanos != 0 {
        let secs = iv.nanos / 1_000_000_000;
        let mins = secs / 60;
        let hours = mins / 60;
        let rem_secs = secs - (mins * 60);
        let rem_mins = mins - (hours * 60);
        let rem_nanos = iv.nanos % 1_000_000_000;
        if hours != 0 {
            let _ = write!(out, "{prefix}{hours} hours");
            prefix = " ";
        }
        if rem_mins != 0 {
            let _ = write!(out, "{prefix}{rem_mins} mins");
            prefix = " ";
        }
        if rem_secs != 0 || rem_nanos != 0 {
            let sign = if rem_secs < 0 || rem_nanos < 0 {
                "-"
            } else {
                ""
            };
            let _ = write!(
                out,
                "{prefix}{sign}{}.{:09} secs",
                rem_secs.unsigned_abs(),
                rem_nanos.unsigned_abs()
            );
        }
    }
    Value::String(out)
}

fn decimal_to_json(value: i128, scale: i8) -> Value {
    let text = if scale == 0 {
        value.to_string()
    } else if scale < 0 {
        if value == 0 {
            "0".to_owned()
        } else {
            let mut s = value.to_string();
            for _ in 0..usize::from(scale.unsigned_abs()) {
                s.push('0');
            }
            s
        }
    } else {
        let neg = value < 0;
        let digits = value.unsigned_abs().to_string();
        let sc = scale as usize;
        let mut out = String::with_capacity(digits.len() + sc + 3);
        if neg {
            out.push('-');
        }
        if digits.len() <= sc {
            out.push_str("0.");
            for _ in 0..(sc - digits.len()) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let split = digits.len() - sc;
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        }
        out
    };
    let num: Number = serde_json::from_str(&text).expect("decimal formats as JSON number");
    Value::Number(num)
}

/// The `trace_id` values of JSON rows, in order.
fn ids_of(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            row["trace_id"]
                .as_str()
                .expect("a trace_id text")
                .to_owned()
        })
        .collect()
}

/// `value` with the keys of every object (a map column, a JSON attribute) in sorted order, so the
/// text of `expected.json` does not depend on the order a read writes its map entries in.
fn sorted_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sorted_keys(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted_keys).collect()),
        other => other,
    }
}

/// Sorts JSON rows by their text: for reads that do not order their rows.
fn sorted(mut rows: Vec<Value>) -> Vec<Value> {
    rows.sort_by_key(Value::to_string);
    rows
}

/// The answers of the public reads over the golden scenario, as JSON. Called on a COPY of the
/// directory only (see `expected_of` and `check_dir`).
pub(super) async fn dump(storage: &Storage) -> Value {
    let mut projects = Map::new();
    for project in [P1, P2] {
        let id: ProjectId = project.parse().expect("a golden project id");
        projects.insert(project.to_owned(), project_dump(storage, id).await);
    }

    // The dropped project: no trace is listed.
    let dropped_id: ProjectId = P3.parse().expect("a golden project id");
    let dropped_context = context(storage, dropped_id).await;
    let dropped_list = trace_list(
        &dropped_context,
        TraceListRequest {
            filter: None,
            search: None,
            scope: scope(),
            strict_exclusions: false,
            sort: TraceSort::StartTime,
            order: SortOrder::Desc,
            limit: MAX_PAGE,
            cursor: None,
            timeout: TIMEOUT,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("trace_list of the dropped project: {error}"));
    let mut dropped = Map::new();
    dropped.insert(
        P3.to_owned(),
        json!({ "trace_list": rows(&dropped_list.batches, TRACE_COLUMNS) }),
    );

    let mut deletions = Map::new();
    for id in [DELETION_TRACE, DELETION_USER] {
        let status = match storage
            .deletion_status(id)
            .await
            .unwrap_or_else(|error| panic!("deletion_status of a golden deletion: {error}"))
        {
            DeletionStatus::Pending => "pending",
            DeletionStatus::Applied { .. } => "applied",
        };
        deletions.insert(id.to_string(), Value::from(status));
    }

    let sha = Sha256::digest(MEDIA);
    let media = storage
        .get_media(
            P1.parse().expect("a golden project id"),
            &sha.into(),
            None,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("get_media of the golden blob: {error}"));

    json!({
        "format_version": FORMAT_VERSION,
        "workspace_version": env!("CARGO_PKG_VERSION"),
        "generator": GENERATOR,
        "windows": { "from": window().0, "to": window().1 },
        "write_probe": { "project": P1, "day": "2026-02-11", "start_time": ns_at("2026-02-11", 3600) },
        "projects": projects,
        "dropped": dropped,
        "deletions": deletions,
        "media": {
            "project": P1,
            "sha256": hex::encode(sha),
            "bytes_len": media.bytes.len(),
        },
        "cache": { "name": CACHE_NAME, "location": CACHE_LOCATION, "size": CACHE_SIZE },
    })
}

/// The columns of a `traces` row the dump keeps.
const TRACE_COLUMNS: &[&str] = &[
    traces::TRACE_ID,
    traces::NAME,
    traces::START_TIME,
    traces::END_TIME,
    traces::SPAN_COUNT,
    traces::ERROR_COUNT,
    traces::TOTAL_TOKENS,
    traces::COST_TOTAL,
    traces::SESSION_ID,
    traces::USER_ID,
];

/// The columns of a `spans` row the dump keeps in lists.
const SPAN_COLUMNS: &[&str] = &[
    spans::TRACE_ID,
    spans::SPAN_ID,
    spans::START_TIME,
    spans::NAME,
    spans::SPAN_KIND,
    spans::STATUS,
    spans::TOTAL_TOKENS,
    spans::COST_TOTAL,
    spans::SESSION_ID,
    spans::USER_ID,
];

/// The columns of a span of a trace detail (the list projection of `trace_detail`).
const DETAIL_COLUMNS: &[&str] = &[
    spans::SPAN_ID,
    spans::PARENT_SPAN_ID,
    spans::NAME,
    spans::SPAN_KIND,
    spans::START_TIME,
    spans::END_TIME,
    spans::STATUS,
    spans::STATUS_MESSAGE,
    spans::MODEL,
    spans::TAGS,
    spans::TOTAL_TOKENS,
];

/// The columns of a span of `span_detail`, the one read with the payload columns (input, output,
/// attributes), which `trace_detail` leaves out.
const PAYLOAD_COLUMNS: &[&str] = &[
    spans::SPAN_ID,
    spans::INPUT,
    spans::OUTPUT,
    spans::ATTRIBUTES,
    spans::OPERATION,
    spans::PROVIDER,
    spans::ENVIRONMENT,
];

/// The columns of a `scores` row the dump keeps.
const SCORE_COLUMNS: &[&str] = &[
    scores::SCORE_ID,
    scores::NAME,
    scores::TARGET,
    scores::TRACE_ID,
    scores::SPAN_ID,
    scores::SESSION_ID,
    scores::VALUE_NUM,
    scores::VALUE_STR,
    scores::REASON,
    scores::CREATED_AT,
];

/// Everything the dump records about one project.
async fn project_dump(storage: &Storage, project: ProjectId) -> Value {
    let traces = all_traces(storage, project).await;
    let trace_ids = ids_of(&traces);

    let mut details = Map::new();
    let mut sorted_ids = trace_ids.clone();
    sorted_ids.sort();
    for id in &sorted_ids {
        let ctx = context(storage, project).await;
        let detail = trace_detail(&ctx, id, TIMEOUT)
            .await
            .unwrap_or_else(|error| panic!("trace_detail of a golden trace: {error}"))
            .expect("a listed trace has a detail");
        let spans_json = rows(&detail.spans, DETAIL_COLUMNS);
        let mut payloads = Vec::new();
        for span in &spans_json {
            let span_id = span["span_id"].as_str().expect("a span_id text");
            let found = span_detail(&ctx, id, span_id, TIMEOUT)
                .await
                .unwrap_or_else(|error| panic!("span_detail of a golden span: {error}"))
                .expect("a listed span has a detail");
            payloads.extend(rows(std::slice::from_ref(&found), PAYLOAD_COLUMNS));
        }
        details.insert(
            id.clone(),
            json!({
                "truncated": detail.truncated,
                "spans": spans_json,
                "span_payloads": payloads,
            }),
        );
    }

    let span_list = all_spans(storage, project).await;
    let scores = all_scores(storage, project).await;

    let ctx = context(storage, project).await;
    let trace_refs: Vec<&str> = sorted_ids.iter().map(String::as_str).collect();
    let latest = latest_scores(&ctx, ScoreTargets::Traces(&trace_refs), TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("latest_scores of golden traces: {error}"));
    let latest_rows = sorted(rows(
        &latest,
        &["target_key", "name", "score_id", "value_num"],
    ));

    let sessions = all_sessions(storage, project).await;
    let users = all_users(storage, project).await;

    let ctx = context(storage, project).await;
    let headline = headline_kpis(&ctx, time_window())
        .await
        .unwrap_or_else(|error| panic!("KPI totals of a golden project: {error}"));
    let kpis = [
        "Traces",
        "ErrorRate",
        "LatencyP95",
        "CostTotal",
        "TotalTokens",
    ]
    .into_iter()
    .zip(headline)
    .map(|(name, (value, previous))| json!({ "name": name, "value": value, "previous": previous }))
    .collect::<Vec<Value>>();

    json!({
        "traces": traces,
        "trace_details": details,
        "spans": span_list,
        "scores": scores,
        "latest_scores": latest_rows,
        "sessions": sessions,
        "users": users,
        "kpis": kpis,
    })
}

/// Every `trace_list` row over the window, in list order, every page followed.
async fn all_traces(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = trace_list(
            &ctx,
            TraceListRequest {
                filter: None,
                search: None,
                scope: scope(),
                strict_exclusions: false,
                sort: TraceSort::StartTime,
                order: SortOrder::Desc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("trace_list of a golden project: {error}"));
        out.extend(rows(&page.batches, TRACE_COLUMNS));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

/// Every `span_list` row over the window, in list order, every page followed.
async fn all_spans(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = span_list(
            &ctx,
            SpanListRequest {
                filter: None,
                search: None,
                scope: scope(),
                strict_exclusions: false,
                sort: SpanSort::StartTime,
                order: SortOrder::Asc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("span_list of a golden project: {error}"));
        out.extend(rows(&page.batches, SPAN_COLUMNS));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

/// Every live `score_list` row over the window, every page followed, sorted by text.
async fn all_scores(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = score_list(
            &ctx,
            ScoreListRequest {
                target: None,
                trace_id: None,
                span_id: None,
                session_id: None,
                name: None,
                source: None,
                evaluator_id: None,
                rule_id: None,
                queue_id: None,
                window: Some(time_window()),
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("score_list of a golden project: {error}"));
        out.extend(rows(&page.batches, SCORE_COLUMNS));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return sorted(out),
        }
    }
}

/// Every `session_list` row over the window, sorted by text.
async fn all_sessions(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = session_list(
            &ctx,
            SessionListRequest {
                filter: None,
                scope: scope(),
                sort: SessionSort::StartTime,
                order: SortOrder::Desc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("session_list of a golden project: {error}"));
        out.extend(rows(
            &page.batches,
            &[
                traces::SESSION_ID,
                "trace_count",
                traces::ERROR_COUNT,
                traces::TOTAL_TOKENS,
                traces::COST_TOTAL,
            ],
        ));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return sorted(out),
        }
    }
}

/// Every `user_list` row over the window, sorted by text.
async fn all_users(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = user_list(
            &ctx,
            UserListRequest {
                filter: None,
                scope: scope(),
                sort: UserSort::TraceCount,
                order: SortOrder::Desc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("user_list of a golden project: {error}"));
        out.extend(rows(&page.batches, &[traces::USER_ID, "trace_count"]));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return sorted(out),
        }
    }
}

/// The KPIs of `expected` plus the contribution of the probe trace row `probe`, which is one more
/// trace of `traces` (the expected trace rows of P1): its cost and its tokens, and its error in
/// ErrorRate, which is the traces with an error over the traces. The probe lasts as long as every
/// golden trace (3 ms), so LatencyP95 does not change.
pub(super) fn kpis_with_probe(expected: &Value, traces: &[Value], probe: &Value) -> Value {
    let cost = probe["cost_total"].as_f64().expect("the probe's cost");
    let tokens = probe["total_tokens"].as_f64().expect("the probe's tokens");
    let with_error = |row: &Value| row["error_count"].as_i64().is_some_and(|count| count > 0);
    let errors = traces.iter().filter(|row| with_error(row)).count();
    let count = traces.len();
    let mut kpis = expected.clone();
    for kpi in kpis.as_array_mut().expect("the KPIs are a list") {
        let value = kpi["value"].as_f64().expect("a golden KPI has a value");
        let added = match kpi["name"].as_str().expect("a KPI name") {
            "Traces" => 1.0,
            "CostTotal" => cost,
            "TotalTokens" => tokens,
            "ErrorRate" => {
                // The rate of the expected traces is exact: the same division `metrics` makes.
                assert_eq!(
                    Some(value),
                    (count > 0).then(|| errors as f64 / count as f64),
                    "the expected ErrorRate is the errors over the traces"
                );
                let probe_error = usize::from(with_error(probe));
                kpi["value"] = Value::from((errors + probe_error) as f64 / (count + 1) as f64);
                continue;
            }
            _ => continue,
        };
        kpi["value"] = Value::from(value + added);
    }
    kpis
}

/// A current-thread runtime for the catalog reads, which are not async in this module.
pub(super) fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime for a catalog read")
        .block_on(future)
}

/// One cell of a catalog row: an integer, text or null.
fn cell(row: &SqliteRow, index: usize) -> Value {
    if let Ok(value) = row.try_get::<Option<i64>, _>(index) {
        return value.map_or(Value::Null, Value::from);
    }
    if let Ok(value) = row.try_get::<Option<String>, _>(index) {
        return value.map_or(Value::Null, Value::from);
    }
    panic!("a catalog column of an unexpected type")
}

/// The rows of `sql` on the catalog file `file`, each row an array of its cells. The file is
/// opened on its own: callers pass a copy, never a live directory.
pub(super) fn catalog_rows(file: &Path, sql: &'static str) -> Vec<Value> {
    let file = file.to_path_buf();
    block_on(async move {
        let options = SqliteConnectOptions::new().filename(file);
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .expect("open a copy of catalog.db");
        let found = sqlx::query(sql)
            .fetch_all(&mut connection)
            .await
            .expect("query a copy of catalog.db");
        // Closing a copy's connection fails only if it is already gone, which changes nothing read here.
        let _ = connection.close().await;
        found
            .iter()
            .map(|row| Value::Array((0..row.len()).map(|index| cell(row, index)).collect()))
            .collect()
    })
}

/// The single integer `sql` returns on the copy of the catalog `file`.
pub(super) fn catalog_count(file: &Path, sql: &'static str) -> i64 {
    let temp = tempfile::tempdir().expect("a temp dir for a catalog copy");
    let copy = temp.path().join("catalog.db");
    fs::copy(file, &copy).expect("copy catalog.db");
    let rows = catalog_rows(&copy, sql);
    rows.first()
        .and_then(|row| row[0].as_i64())
        .expect("the query returns one integer")
}

/// The catalog of the directory `dir`: its data files by table, level and state, the retention
/// watermarks, the projects and media rows, and the number of applied migrations. Read from a
/// copy of `catalog.db` before any `Storage` opens the directory. Not the flushed sequence, file
/// ids or timestamps: storage assigns those.
pub(super) fn catalog_summary(dir: &Path) -> Value {
    let temp = tempfile::tempdir().expect("a temp dir for a catalog copy");
    let copy = temp.path().join("catalog.db");
    fs::copy(dir.join("catalog.db"), &copy).expect("copy catalog.db");
    let migrations = catalog_rows(&copy, "SELECT COUNT(*) FROM _sqlx_migrations");
    json!({
        "data_files": catalog_rows(
            &copy,
            "SELECT table_name, level, state, COUNT(*) FROM data_files \
             GROUP BY table_name, level, state ORDER BY table_name, level, state",
        ),
        "retention_watermarks": catalog_rows(
            &copy,
            "SELECT project_id, keep_from FROM retention_watermarks ORDER BY project_id",
        ),
        "projects": catalog_rows(&copy, "SELECT project_id, state FROM projects ORDER BY project_id"),
        "media": catalog_rows(
            &copy,
            "SELECT project_id, sha256, mime_type, size_bytes FROM media ORDER BY project_id, sha256",
        ),
        "applied_migrations": migrations[0][0].clone(),
    })
}

/// The `expected.json` of the directory `dir`: the dump of a copy, plus its catalog summary
/// (taken from the copy before it opens). `dir` is not changed.
pub(super) fn expected_of(dir: &Path) -> Value {
    let temp = tempfile::tempdir().expect("a temp dir for the copy");
    let root = temp.path().join("golden");
    crate::test_support::copy_dir(dir, &root);
    let catalog = catalog_summary(&root);
    let config = super::build::golden_config(&root);
    let mut expected =
        crate::test_support::clean_life(&config, async |storage| dump(storage).await);
    expected
        .as_object_mut()
        .expect("the dump is an object")
        .insert("catalog".to_owned(), catalog);
    expected
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, DictionaryArray,
        FixedSizeBinaryArray, Float64Array, Int32Array, Int64Array, IntervalMonthDayNanoArray,
        ListArray, MapArray, RecordBatch, RunArray, StringArray, StructArray,
        TimestampNanosecondArray,
    };
    use arrow::buffer::{NullBuffer, OffsetBuffer};
    use arrow::datatypes::{
        DataType as ArrowDataType, Field as ArrowField, Fields, IntervalMonthDayNano, IntervalUnit,
        Schema as ArrowSchema, TimeUnit,
    };
    use cetra_column::{
        Batch, Bitmap, Column, DataType as ColDataType, DictColumn, Field as ColField, FlatColumn,
        FlatData, RleColumn, ScalarValue, Schema as ColSchema, Selection, Utf8Data,
    };
    use serde_json::Value;

    use super::{batch_rows, rows, sorted_keys};

    fn arrow_writer_rows(batch: &RecordBatch) -> Vec<Value> {
        let mut writer = arrow::json::ArrayWriter::new(Vec::new());
        writer.write(batch).expect("arrow json write");
        writer.finish().expect("arrow json finish");
        let Value::Array(values) =
            serde_json::from_slice(&writer.into_inner()).expect("parse arrow json")
        else {
            panic!("ArrayWriter must emit a JSON array");
        };
        values.into_iter().map(sorted_keys).collect()
    }

    #[test]
    fn batch_json_formatter_matches_arrow_array_writer_across_all_types() {
        let trace_a = [0x11_u8; 16];
        let trace_b = [0xab_u8; 16];
        let span_a = [0x01_u8; 8];
        let span_b = [0xfe_u8; 8];

        let map_entries_fields = Fields::from(vec![
            ArrowField::new("keys", ArrowDataType::Utf8, false),
            ArrowField::new("values", ArrowDataType::Utf8, true),
        ]);
        let map_field = Arc::new(ArrowField::new(
            "entries",
            ArrowDataType::Struct(map_entries_fields.clone()),
            false,
        ));
        let map_struct = StructArray::try_new(
            map_entries_fields,
            vec![
                Arc::new(StringArray::from(vec!["z", "a", "null_val"])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("1"), Some("2"), None])) as ArrayRef,
            ],
            None,
        )
        .unwrap();
        let map_arr = MapArray::try_new(
            Arc::clone(&map_field),
            OffsetBuffer::new(vec![0, 3, 3, 3].into()),
            map_struct,
            Some(NullBuffer::from(vec![true, true, false])),
            false,
        )
        .unwrap();

        let list_arr = ListArray::try_new(
            Arc::new(ArrowField::new("item", ArrowDataType::Utf8, true)),
            OffsetBuffer::new(vec![0, 2, 2, 2].into()),
            Arc::new(StringArray::from(vec![Some("tag1"), None])),
            Some(NullBuffer::from(vec![true, true, false])),
        )
        .unwrap();

        let event_fields = Fields::from(vec![
            ArrowField::new(
                "time",
                ArrowDataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                false,
            ),
            ArrowField::new("name", ArrowDataType::Utf8, false),
            ArrowField::new("attributes", ArrowDataType::Utf8, true),
        ]);
        let events_struct = StructArray::try_new(
            event_fields.clone(),
            vec![
                Arc::new(
                    TimestampNanosecondArray::from(vec![1_770_717_600_000_000_000_i64])
                        .with_timezone("UTC"),
                ) as ArrayRef,
                Arc::new(StringArray::from(vec!["evt"])) as ArrayRef,
                Arc::new(StringArray::from(vec![Option::<&str>::None])) as ArrayRef,
            ],
            None,
        )
        .unwrap();
        let events_arr = ListArray::try_new(
            Arc::new(ArrowField::new(
                "item",
                ArrowDataType::Struct(event_fields),
                false,
            )),
            OffsetBuffer::new(vec![0, 1, 1, 1].into()),
            Arc::new(events_struct),
            Some(NullBuffer::from(vec![true, true, false])),
        )
        .unwrap();

        let link_fields = Fields::from(vec![
            ArrowField::new("trace_id", ArrowDataType::FixedSizeBinary(16), false),
            ArrowField::new("span_id", ArrowDataType::FixedSizeBinary(8), false),
            ArrowField::new("attributes", ArrowDataType::Utf8, true),
        ]);
        let links_struct = StructArray::try_new(
            link_fields.clone(),
            vec![
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter([trace_a.as_slice()].into_iter()).unwrap(),
                ) as ArrayRef,
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter([span_a.as_slice()].into_iter()).unwrap(),
                ) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("{\"k\":1}")])) as ArrayRef,
            ],
            None,
        )
        .unwrap();
        let links_arr = ListArray::try_new(
            Arc::new(ArrowField::new(
                "item",
                ArrowDataType::Struct(link_fields),
                false,
            )),
            OffsetBuffer::new(vec![0, 1, 1, 1].into()),
            Arc::new(links_struct),
            Some(NullBuffer::from(vec![true, true, false])),
        )
        .unwrap();

        let dict_arr: DictionaryArray<arrow::datatypes::Int32Type> =
            vec![Some("ok"), None, Some("error")].into_iter().collect();
        let run_arr = RunArray::try_new(
            &Int32Array::from(vec![2, 3]),
            &StringArray::from(vec![Some("rle-a"), Some("rle-b")]),
        )
        .unwrap();

        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("b", ArrowDataType::Boolean, true),
            ArrowField::new("i", ArrowDataType::Int64, true),
            ArrowField::new("f", ArrowDataType::Float64, true),
            ArrowField::new("dec", ArrowDataType::Decimal128(38, 18), true),
            ArrowField::new(
                "ts_utc",
                ArrowDataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                true,
            ),
            ArrowField::new(
                "ts_naive",
                ArrowDataType::Timestamp(TimeUnit::Nanosecond, None),
                true,
            ),
            ArrowField::new("d", ArrowDataType::Date32, true),
            ArrowField::new(
                "iv",
                ArrowDataType::Interval(IntervalUnit::MonthDayNano),
                true,
            ),
            ArrowField::new("s", ArrowDataType::Utf8, true),
            ArrowField::new("bin", ArrowDataType::Binary, true),
            ArrowField::new("tid", ArrowDataType::FixedSizeBinary(16), true),
            ArrowField::new("sid", ArrowDataType::FixedSizeBinary(8), true),
            ArrowField::new("tags", list_arr.data_type().clone(), true),
            ArrowField::new("attrs", map_arr.data_type().clone(), true),
            ArrowField::new("events", events_arr.data_type().clone(), true),
            ArrowField::new("links", links_arr.data_type().clone(), true),
            ArrowField::new("dict_col", dict_arr.data_type().clone(), true),
            ArrowField::new("rle_col", run_arr.data_type().clone(), true),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
                Arc::new(Int64Array::from(vec![Some(-42), Some(0), None])),
                Arc::new(Float64Array::from(vec![Some(1.25), Some(0.0), None])),
                Arc::new(
                    Decimal128Array::from(vec![
                        Some(123_450_000_000_000_000_i128),
                        Some(-1_000_000_000_000_000_000_i128),
                        None,
                    ])
                    .with_precision_and_scale(38, 18)
                    .unwrap(),
                ),
                Arc::new(
                    TimestampNanosecondArray::from(vec![
                        Some(1_770_717_600_000_000_000_i64), // 2026-02-10T10:00:00Z
                        Some(1_770_717_600_100_000_001_i64), // .100000001Z
                        None,
                    ])
                    .with_timezone("UTC"),
                ),
                Arc::new(TimestampNanosecondArray::from(vec![
                    Some(1_770_717_600_000_000_000_i64),
                    Some(1_770_717_600_100_000_000_i64),
                    None,
                ])),
                Arc::new(Date32Array::from(vec![Some(20_494), Some(0), None])),
                Arc::new(IntervalMonthDayNanoArray::from(vec![
                    Some(IntervalMonthDayNano::new(0, 0, 0)),
                    Some(IntervalMonthDayNano::new(14, -2, -1_500_000_000)),
                    None,
                ])),
                Arc::new(StringArray::from(vec![
                    Some("hello\n\"world\""),
                    Some(""),
                    None,
                ])),
                Arc::new(BinaryArray::from(vec![
                    Some(b"\x00\xff".as_slice()),
                    Some(b"".as_slice()),
                    None,
                ])),
                Arc::new(
                    FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                        [Some(trace_a.as_slice()), Some(trace_b.as_slice()), None].into_iter(),
                        16,
                    )
                    .unwrap(),
                ),
                Arc::new(
                    FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                        [Some(span_a.as_slice()), Some(span_b.as_slice()), None].into_iter(),
                        8,
                    )
                    .unwrap(),
                ),
                Arc::new(list_arr),
                Arc::new(map_arr),
                Arc::new(events_arr),
                Arc::new(links_arr),
                Arc::new(dict_arr),
                Arc::new(run_arr),
            ],
        )
        .unwrap();

        let batch_schema = batch.schema();
        let col_names: Vec<&str> = batch_schema
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect();
        let actual = rows(std::slice::from_ref(&batch), &col_names);
        let expected = arrow_writer_rows(&batch);
        assert_eq!(actual, expected);
        assert_eq!(
            actual[0]["ts_utc"],
            Value::String("2026-02-10T10:00:00Z".to_owned())
        );
    }

    #[test]
    fn batch_json_formatter_handles_const_dict_rle_and_selections() {
        let schema = ColSchema::try_new_arc(vec![
            ColField::new("c", ColDataType::Utf8, true),
            ColField::new("d", ColDataType::I64, true),
            ColField::new("r", ColDataType::FixedBytes8, true),
        ])
        .unwrap();
        let const_col = Column::Const {
            value: ScalarValue::Utf8(String::from("const-v")),
            len: 4,
        };
        let dict_vals = FlatColumn::try_new(FlatData::I64(vec![10, 20]), None).unwrap();
        let dict_col = Column::Dict(
            DictColumn::try_new(
                dict_vals,
                vec![0, 1, 0, 1],
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap(),
        );
        let rle_vals = FlatColumn::try_new(
            FlatData::FixedBytes8(vec![[0x01; 8], [0x02; 8]]),
            Some(Bitmap::from_bools(&[true, false])),
        )
        .unwrap();
        let rle_col = Column::Rle(RleColumn::try_new(rle_vals, vec![2, 4]).unwrap());

        let base =
            Batch::try_new_all(Arc::clone(&schema), vec![const_col, dict_col, rle_col], 4).unwrap();

        let masked = base
            .with_selection(Selection::Mask(Bitmap::from_bools(&[
                false, true, true, false,
            ])))
            .unwrap();
        let mask_rows = batch_rows(&masked, &["c", "d", "r"]);
        assert_eq!(mask_rows.len(), 2);
        assert_eq!(
            mask_rows[0],
            serde_json::json!({"c": "const-v", "r": "0101010101010101"})
        );
        assert_eq!(mask_rows[1], serde_json::json!({"c": "const-v", "d": 10}));

        let indexed = base.with_selection(Selection::Indices(vec![3, 0])).unwrap();
        let idx_rows = batch_rows(&indexed, &["c", "d", "r"]);
        assert_eq!(idx_rows.len(), 2);
        assert_eq!(idx_rows[0], serde_json::json!({"c": "const-v", "d": 20}));
        assert_eq!(
            idx_rows[1],
            serde_json::json!({"c": "const-v", "d": 10, "r": "0101010101010101"})
        );

        let _ = Utf8Data::empty();
    }
}
