//! Differential parity tests comparing `cetra-query` against DataFusion (D-QE-9).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, Float64Builder, RecordBatch, StringBuilder,
};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema};
use cetra_column::{Batch, CancelToken, DataType, Field, Reservation, ScalarValue, Schema};
use cetra_core::schema::{scores_schema, spans_schema, traces_schema};
use cetra_core::score::{Score, ScoreTarget};
use cetra_query::{
    EvalContext, FilterView, TombstoneSpec, TombstoneTable, bind_expr, compile_filter, eval_expr,
    eval_filter,
};
use uuid::Uuid;

use crate::catalog::FileTable;
use crate::column_bridge::record_batch_to_column;
use crate::deletion::{
    PendingDeletion, covered_predicate as df_covered_predicate, drop_covered as df_drop_covered,
};
use crate::filter::FilterView as DfFilterView;
use crate::test_support::{
    dataset_scores, example_query, live_scores, score_row, score_rows, span_row, span_rows,
    trace_batch,
};
use crate::views::QueryContext;

fn query_ctx() -> QueryContext {
    QueryContext::new(example_query(), None).expect("example project")
}

fn eval_ctx() -> EvalContext {
    EvalContext {
        now_ns: chrono::Utc::now()
            .timestamp_nanos_opt()
            .expect("current timestamp in ns"),
        cancel: CancelToken::new(),
    }
}

async fn collect_view_batch(ctx: &QueryContext, table: &str) -> (RecordBatch, Batch) {
    let batches = ctx
        .session()
        .table(table)
        .await
        .expect("view table")
        .collect()
        .await
        .expect("collect view");
    let rb = concat_batches(&batches[0].schema(), &batches).expect("concat batches");
    let cb = record_batch_to_column(&rb).expect("column bridge");
    (rb, cb)
}

fn extract_id_column(rb: &RecordBatch, col_name: &str) -> Vec<String> {
    let arr = rb.column_by_name(col_name).expect("id column");
    if let Some(s) = arr.as_string_opt::<i32>() {
        return (0..s.len()).map(|i| s.value(i).to_owned()).collect();
    }
    if let Some(s) = arr.as_string_view_opt() {
        return (0..s.len()).map(|i| s.value(i).to_owned()).collect();
    }
    panic!("unexpected id column type: {}", arr.data_type());
}

fn to_df_view(view: FilterView) -> DfFilterView {
    match view {
        FilterView::Traces => DfFilterView::Traces,
        FilterView::Spans => DfFilterView::Spans,
        FilterView::Sessions => DfFilterView::Sessions,
        FilterView::Scores => DfFilterView::Scores,
    }
}

fn df_view_name(view: FilterView) -> &'static str {
    match view {
        FilterView::Traces => "traces",
        FilterView::Spans => "spans",
        FilterView::Sessions => "sessions",
        FilterView::Scores => "scores",
    }
}

fn df_id_column(view: FilterView) -> &'static str {
    match view {
        FilterView::Traces => "trace_id",
        FilterView::Spans => "span_id",
        FilterView::Sessions => "session_id",
        FilterView::Scores => "score_id",
    }
}

async fn df_matching_ids(ctx: &QueryContext, view: FilterView, text: &str) -> BTreeSet<String> {
    let filter = ctx
        .parse_filter(to_df_view(view), text)
        .unwrap_or_else(|e| panic!("df parse `{text}`: {e} at {:?}", e.position));
    let frame = ctx
        .session()
        .table(df_view_name(view))
        .await
        .expect("table");
    let filtered = filter
        .apply(frame, ctx)
        .await
        .unwrap_or_else(|e| panic!("df apply `{text}`: {e}"));
    let batches = filtered
        .collect()
        .await
        .unwrap_or_else(|e| panic!("df collect `{text}`: {e}"));
    let id_col = df_id_column(view);
    let mut out = BTreeSet::new();
    for b in &batches {
        let arr = b.column_by_name(id_col).expect("id col");
        if let Some(s) = arr.as_string_opt::<i32>() {
            out.extend(s.iter().flatten().map(str::to_owned));
        } else if let Some(s) = arr.as_string_view_opt() {
            out.extend(s.iter().flatten().map(str::to_owned));
        } else {
            panic!("unexpected id type {}", arr.data_type());
        }
    }
    out
}

/// Joins the latest score of `score_name` (matching `filter/scores.rs` `(updated_at, score_id)` tie-breaker)
/// onto a view `RecordBatch` as `__score_{index}_{num,str,present}` columns.
fn append_prejoined_score_columns(
    rb: &RecordBatch,
    view: FilterView,
    scores: &[Score],
    score_names: &[&str],
) -> (RecordBatch, Batch) {
    let mut fields: Vec<Arc<ArrowField>> = rb.schema().fields().iter().cloned().collect();
    let mut arrays: Vec<ArrayRef> = rb.columns().to_vec();
    let n_rows = rb.num_rows();

    for (index, &score_name) in score_names.iter().enumerate() {
        let mut best_by_key: BTreeMap<String, &Score> = BTreeMap::new();
        for s in scores {
            if s.name != score_name {
                continue;
            }
            let key = match (view, &s.target) {
                (FilterView::Traces, ScoreTarget::Trace { trace_id }) => trace_id.to_hex(),
                (FilterView::Spans, ScoreTarget::Span { trace_id, span_id }) => {
                    format!("{}:{}", trace_id.to_hex(), span_id.to_hex())
                }
                (FilterView::Sessions, ScoreTarget::Session { session_id }) => session_id.clone(),
                _ => continue,
            };
            let replace = best_by_key
                .get(&key)
                .is_none_or(|cur| (s.updated_at, s.score_id) > (cur.updated_at, cur.score_id));
            if replace {
                best_by_key.insert(key, s);
            }
        }

        let mut num_b = Float64Builder::with_capacity(n_rows);
        let mut str_b = StringBuilder::new();
        let mut pres_b = arrow::array::BooleanBuilder::with_capacity(n_rows);

        let trace_ids = if matches!(view, FilterView::Traces | FilterView::Spans) {
            Some(extract_id_column(rb, "trace_id"))
        } else {
            None
        };
        let span_ids = if view == FilterView::Spans {
            Some(extract_id_column(rb, "span_id"))
        } else {
            None
        };
        let session_ids = if view == FilterView::Sessions {
            Some(extract_id_column(rb, "session_id"))
        } else {
            None
        };

        for row in 0..n_rows {
            let key = match view {
                FilterView::Traces => trace_ids.as_ref().expect("trace_ids")[row].clone(),
                FilterView::Spans => format!(
                    "{}:{}",
                    trace_ids.as_ref().expect("trace_ids")[row],
                    span_ids.as_ref().expect("span_ids")[row]
                ),
                FilterView::Sessions => session_ids.as_ref().expect("session_ids")[row].clone(),
                FilterView::Scores => unreachable!(),
            };
            match best_by_key.get(&key) {
                Some(sc) => {
                    match sc.value.value_num() {
                        Some(v) => num_b.append_value(v),
                        None => num_b.append_null(),
                    }
                    match sc.value.value_str() {
                        Some(v) => str_b.append_value(v),
                        None => str_b.append_null(),
                    }
                    pres_b.append_value(true);
                }
                None => {
                    num_b.append_null();
                    str_b.append_null();
                    pres_b.append_null();
                }
            }
        }

        fields.push(Arc::new(ArrowField::new(
            format!("__score_{index}_num"),
            ArrowDataType::Float64,
            true,
        )));
        fields.push(Arc::new(ArrowField::new(
            format!("__score_{index}_str"),
            ArrowDataType::Utf8,
            true,
        )));
        fields.push(Arc::new(ArrowField::new(
            format!("__score_{index}_present"),
            ArrowDataType::Boolean,
            true,
        )));
        arrays.push(Arc::new(num_b.finish()));
        arrays.push(Arc::new(str_b.finish()));
        arrays.push(Arc::new(pres_b.finish()));
    }

    let schema = Arc::new(ArrowSchema::new(fields));
    let joined_rb = RecordBatch::try_new(schema, arrays).expect("joined record batch");
    let joined_cb = record_batch_to_column(&joined_rb).expect("joined column batch");
    (joined_rb, joined_cb)
}

#[tokio::test]
async fn filter_dsl_matches_datafusion_row_for_row() {
    let df_ctx = query_ctx();
    let e_ctx = eval_ctx();

    let (spans_rb, spans_cb) = collect_view_batch(&df_ctx, "spans").await;
    let (traces_rb, traces_cb) = collect_view_batch(&df_ctx, "traces").await;
    let (scores_rb, scores_cb) = collect_view_batch(&df_ctx, "scores").await;

    let span_ids = extract_id_column(&spans_rb, "span_id");
    let trace_ids = extract_id_column(&traces_rb, "trace_id");
    let score_ids = extract_id_column(&scores_rb, "score_id");
    assert_eq!(
        span_ids.iter().collect::<HashSet<_>>().len(),
        span_ids.len()
    );
    assert_eq!(
        trace_ids.iter().collect::<HashSet<_>>().len(),
        trace_ids.len()
    );
    assert_eq!(
        score_ids.iter().collect::<HashSet<_>>().len(),
        score_ids.len()
    );

    // 45+ Filter DSL differential cases across Spans, Traces, and Scores views
    let cases: &[(FilterView, &str)] = &[
        // Spans: equality, inequality, ranges, scientific notation, NULLs, booleans
        (FilterView::Spans, "status = 'error'"),
        (FilterView::Spans, "status != 'error'"),
        (
            FilterView::Spans,
            "duration_ns > 0 AND duration_ns <= 5000000000",
        ),
        (FilterView::Spans, "duration_ns BETWEEN 1e8 AND 5e9"),
        (FilterView::Spans, "duration_ns NOT BETWEEN 1e8 AND 5e9"),
        (
            FilterView::Spans,
            "duration_ms >= +10.0 AND -duration_ns < 0",
        ),
        (FilterView::Spans, "span_kind IN ('llm', 'tool')"),
        (FilterView::Spans, "span_kind NOT IN ('llm', 'tool')"),
        (FilterView::Spans, "environment IN ('production', NULL)"),
        (FilterView::Spans, "environment NOT IN ('production', NULL)"),
        (FilterView::Spans, "environment IS NULL"),
        (FilterView::Spans, "environment IS NOT NULL"),
        (FilterView::Spans, "environment IS DISTINCT FROM 'eval'"),
        (FilterView::Spans, "environment IS NOT DISTINCT FROM NULL"),
        // LIKE / ILIKE (default escape & explicit ESCAPE '\\')
        (FilterView::Spans, "model LIKE 'claude%'"),
        (FilterView::Spans, "model NOT LIKE 'claude%'"),
        (FilterView::Spans, "input ILIKE '%order%'"),
        (FilterView::Spans, "input NOT ILIKE '%order%'"),
        (FilterView::Spans, "name LIKE '%\\_%' ESCAPE '\\'"),
        (FilterView::Spans, "name ILIKE 'llm\\_%' ESCAPE '\\'"),
        // Maps & arrays (including empty [] and dynamic lower('BETA'))
        (
            FilterView::Spans,
            "attributes['cetra.eval.evaluator_id'] = '01900000-0000-7000-8000-000000000201'",
        ),
        (FilterView::Spans, "attributes['nonexistent_key'] IS NULL"),
        (FilterView::Spans, "tags @> []"),
        (FilterView::Spans, "tags @> [lower('BETA')]"),
        (FilterView::Spans, "array_has(tags, 'alpha')"),
        (FilterView::Spans, "array_length(tags) >= 1"),
        // Casts (rewritten to TRY_CAST: valid & invalid runtime conversions)
        (FilterView::Spans, "CAST(duration_ns AS DOUBLE) > 0"),
        (
            FilterView::Spans,
            "attributes['cetra.eval.evaluator_id']::DOUBLE > 0",
        ),
        (FilterView::Spans, "name::BIGINT IS NULL"),
        (FilterView::Spans, "duration_ns::VARCHAR != ''"),
        // All 9 Filter DSL scalar functions on Spans
        (FilterView::Spans, "lower(name) = name"),
        (
            FilterView::Spans,
            "upper(status) = 'ERROR' OR upper(status) = 'OK'",
        ),
        (FilterView::Spans, "length(name) > 3"),
        (FilterView::Spans, "coalesce(environment, 'prod') = 'prod'"),
        (FilterView::Spans, "abs(duration_ns) > 0"),
        (
            FilterView::Spans,
            "date_trunc('hour', start_time) >= '2026-01-01T00:00:00Z'",
        ),
        (
            FilterView::Spans,
            "start_time > now() - INTERVAL '3650 days'",
        ),
        (FilterView::Spans, "\"name\" = name AND NAME = name"),
        // Traces view
        (
            FilterView::Traces,
            "cost_total > 0.001 AND tags @> ['beta']",
        ),
        (FilterView::Traces, "metadata['channel'] = 'email'"),
        (FilterView::Traces, "metadata['channel']::DOUBLE > 0"),
        (FilterView::Traces, "NOT has_root"),
        (FilterView::Traces, "start_time >= '2026-01-01T10:05:00Z'"),
        (
            FilterView::Traces,
            "date_trunc('day', start_time) >= CAST('2026-01-01T00:00:00Z' AS TIMESTAMP)",
        ),
        (
            FilterView::Traces,
            "start_time > now() - INTERVAL '3650 days'",
        ),
        (
            FilterView::Traces,
            "tags @> [] AND array_length(models) >= 0",
        ),
        // Scores view
        (
            FilterView::Scores,
            "name = 'helpfulness' AND value_num > 0.5",
        ),
        (
            FilterView::Scores,
            "value_str IS NOT NULL OR value_num IS NULL",
        ),
    ];
    assert!(
        cases.len() >= 40,
        "expected >= 40 cases, got {}",
        cases.len()
    );

    for &(view, text) in cases {
        let (batch, ids, base_schema) = match view {
            FilterView::Spans => (&spans_cb, &span_ids, spans_cb.schema()),
            FilterView::Traces => (&traces_cb, &trace_ids, traces_cb.schema()),
            FilterView::Scores => (&scores_cb, &score_ids, scores_cb.schema()),
            FilterView::Sessions => unreachable!(),
        };

        let df_parsed = df_ctx
            .parse_filter(to_df_view(view), text)
            .unwrap_or_else(|e| panic!("df parse `{text}`: {e}"));
        let cq_compiled = compile_filter(view, base_schema, text)
            .unwrap_or_else(|e| panic!("cq compile `{text}`: {e} at {:?}", e.position));

        assert_eq!(
            cq_compiled.references_environment, df_parsed.references_environment,
            "references_environment mismatch for `{text}`"
        );
        assert_eq!(
            cq_compiled.scores, df_parsed.scores,
            "scores mismatch for `{text}`"
        );

        let df_matched = df_matching_ids(&df_ctx, view, text).await;
        let cq_mask = eval_filter(&cq_compiled.expr, batch, &e_ctx, None)
            .unwrap_or_else(|e| panic!("cq eval `{text}`: {e}"));

        let mut cq_matched = BTreeSet::new();
        for (row, id) in ids.iter().enumerate() {
            let expected = df_matched.contains(id);
            let actual = cq_mask.get(row);
            assert_eq!(
                actual, expected,
                "row {row} ({id}) mask mismatch for `{text}`"
            );
            if actual {
                cq_matched.insert(id.clone());
            }
        }
        assert_eq!(cq_matched, df_matched, "matched set mismatch for `{text}`");
    }

    // Score functions (`score`, `score_label`, `has_score`) with pre-joined `__score_0_*` batches
    let live = live_scores(&dataset_scores());
    let score_cases: &[(FilterView, &[&str], &str)] = &[
        (
            FilterView::Traces,
            &["helpfulness"],
            "score('helpfulness') >= 0.5",
        ),
        (
            FilterView::Traces,
            &["helpfulness"],
            "score('helpfulness') IS NULL",
        ),
        (
            FilterView::Traces,
            &["helpfulness"],
            "has_score('helpfulness')",
        ),
        (
            FilterView::Traces,
            &["helpfulness"],
            "NOT has_score('helpfulness')",
        ),
        (
            FilterView::Traces,
            &["helpfulness", "thumbs"],
            "score('helpfulness') >= 0.5 OR score('thumbs') >= 0.5",
        ),
        (
            FilterView::Traces,
            &["helpfulness", "thumbs"],
            "score('helpfulness') >= 0.5 AND score('thumbs') >= 0.5",
        ),
        (
            FilterView::Spans,
            &["tone"],
            "score_label('tone') = 'friendly'",
        ),
        (FilterView::Spans, &["tone"], "score('tone') > 0.5"),
    ];

    for &(view, score_names, text) in score_cases {
        let (base_rb, base_schema, ids) = match view {
            FilterView::Traces => (&traces_rb, traces_cb.schema(), &trace_ids),
            FilterView::Spans => (&spans_rb, spans_cb.schema(), &span_ids),
            _ => unreachable!(),
        };
        let (_joined_rb, joined_cb) =
            append_prejoined_score_columns(base_rb, view, &live, score_names);
        let cq_compiled = compile_filter(view, base_schema, text)
            .unwrap_or_else(|e| panic!("score compile `{text}`: {e}"));
        assert_eq!(cq_compiled.planning_schema, **joined_cb.schema());
        let cq_mask = eval_filter(&cq_compiled.expr, &joined_cb, &e_ctx, None)
            .unwrap_or_else(|e| panic!("score eval `{text}`: {e}"));
        let df_matched = df_matching_ids(&df_ctx, view, text).await;

        for (row, id) in ids.iter().enumerate() {
            assert_eq!(
                cq_mask.get(row),
                df_matched.contains(id),
                "score row {row} ({id}) mismatch for `{text}`"
            );
        }
    }
}

#[tokio::test]
async fn sql_scalar_expressions_match_datafusion_row_for_row() {
    let df_ctx = query_ctx();
    let e_ctx = eval_ctx();
    let (spans_rb, spans_cb) = collect_view_batch(&df_ctx, "spans").await;
    let span_ids = extract_id_column(&spans_rb, "span_id");
    let schema = spans_cb.schema();

    // Additional Shape B SQL scalar expressions (functions & operators beyond Filter DSL)
    let sql_predicates = [
        "substr(name, 1, 3) = 'llm' OR substr(name, -1, 4) = 'ro'",
        "substring(name, 2, 2) = 'oo'",
        "char_length(name) = length(name)",
        "trim(name) = ltrim(rtrim(name))",
        "left(name, 2) = substr(name, 1, 2)",
        "right(name, 2) != ''",
        "split_part(name, '_', 1) != ''",
        "concat(status, ':', duration_ns) LIKE '%:%'",
        "concat_ws('|', status, environment, NULL) != ''",
        "starts_with(name, 'llm') = (name LIKE 'llm%')",
        "ends_with(status, 'k') = (status LIKE '%k')",
        "contains(name, 'o') = (strpos(name, 'o') > 0)",
        "POSITION('o' IN name) = strpos(name, 'o')",
        "regexp_like(name, '^[a-z]', 'i')",
        "encode(name, 'hex') != ''",
        "date_bin(INTERVAL '1 minute', start_time, TIMESTAMP '1970-01-01T00:00:00Z') <= start_time",
        "EXTRACT(YEAR FROM start_time) = 2026",
        "date_part('epoch', start_time) > 0",
        "round(duration_ms, 1) >= 0",
        "ceil(duration_ms) >= floor(duration_ms)",
        "sqrt(abs(duration_ms)) >= 0",
        "power(2, 3) = 8 AND pow(4, 0.5) = 2",
        "nullif(status, 'ok') IS NULL OR status != 'ok'",
        "greatest(duration_ns, 0, NULL) >= 0 AND least(duration_ns, 1000000000000, NULL) <= 1000000000000",
        "NOT isnan(duration_ms)",
        "array_contains(tags, 'beta') = array_has(tags, 'beta')",
        "array_has_all(tags, ['beta']) = (tags @> ['beta'])",
        "array_element(tags, -1) IS NULL OR array_length(tags) > 0",
        "array_length(array_distinct(tags)) <= array_length(tags)",
        "array_to_string(tags, ',', 'null') IS NOT NULL",
        "(tags || ['extra']) @> ['extra']",
        "(status || '_x') = concat(status, '_x')",
        "CASE WHEN duration_ns > 0 THEN duration_ns * 2 ELSE 0 END >= 0",
        "TRY_CAST(name AS DOUBLE) IS NULL",
    ];

    let tokens_parser_bind = |sql: &str| {
        let tokens =
            cetra_query::text::tokenize(sql, &cetra_query::SQL_LIMITS, cetra_query::TextKind::Sql)
                .unwrap();
        let mut p = cetra_query::text::parser(tokens);
        let ast = p.parse_expr().unwrap();
        cetra_query::text::expect_end(&mut p, sql).unwrap();
        bind_expr(&ast, schema).unwrap()
    };

    for pred in sql_predicates {
        let expr = tokens_parser_bind(pred);
        let cq_mask = eval_filter(&expr, &spans_cb, &e_ctx, None)
            .unwrap_or_else(|e| panic!("cq sql eval `{pred}`: {e}"));

        let sql = format!("SELECT span_id FROM spans WHERE {pred}");
        let df_batches = df_ctx
            .session()
            .sql(&sql)
            .await
            .unwrap_or_else(|e| panic!("df sql `{sql}`: {e}"))
            .collect()
            .await
            .unwrap_or_else(|e| panic!("df collect `{sql}`: {e}"));

        let mut df_matched = BTreeSet::new();
        for b in &df_batches {
            df_matched.extend(extract_id_column(b, "span_id"));
        }
        for (row, id) in span_ids.iter().enumerate() {
            assert_eq!(
                cq_mask.get(row),
                df_matched.contains(id),
                "sql differential mismatch row {row} ({id}) for `{pred}`"
            );
        }
    }

    // `regexp_extract(status, '(ok|error)')` parity with DataFusion `regexp_match(status, '(ok|error)')[1]`
    let re_expr = tokens_parser_bind("regexp_extract(status, '(ok|error)') = 'ok'");
    let re_mask = eval_filter(&re_expr, &spans_cb, &e_ctx, None).unwrap();
    let df_re_batches = df_ctx
        .session()
        .sql("SELECT span_id FROM spans WHERE regexp_match(status, '(ok|error)')[1] = 'ok'")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let mut df_re_matched = BTreeSet::new();
    for b in &df_re_batches {
        df_re_matched.extend(extract_id_column(b, "span_id"));
    }
    for (row, id) in span_ids.iter().enumerate() {
        assert_eq!(re_mask.get(row), df_re_matched.contains(id));
    }
}

#[test]
fn invalid_filters_match_datafusion_errors() {
    let df_ctx = query_ctx();
    let span_arrow_schema = spans_schema();
    // Use view schema via column_bridge on empty batch or by building from DataFusion's schema:
    let _ = span_arrow_schema;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (spans_rb, spans_cb) = rt.block_on(collect_view_batch(&df_ctx, "spans"));
    let _ = spans_rb;
    let view_schema = spans_cb.schema();

    // Exact message + position parity cases (22 cases)
    let exact_cases = [
        "",
        " \n\t",
        "nme = 'a'",
        "spans.name = 'a'",
        "trace_id IN (SELECT trace_id FROM spans)",
        "EXISTS (SELECT 1)",
        "CASE WHEN true THEN true END",
        "sleep(1) > 0",
        "count(*) > 1",
        "lower(DISTINCT name) = 'a'",
        "row_number() OVER () > 1",
        "name = 'a' extra",
        "cost_total + 1",
        "name['k'] = 'a'",
        "metadata[1] = 'a'",
        "CAST(name AS INT) = 1",
        "TRY_CAST(name AS BIGINT) = 1",
        "tags @> ARRAY['beta']",
        "now(1) > start_time",
        "x'00' = name",
        "name || 'x' = 'ax'",
        "name LIKE ANY ('a%')",
        "score(name) > 1",
        "__score_0_num > 1",
    ];

    for text in exact_cases {
        let df_err = df_ctx
            .parse_filter(DfFilterView::Spans, text)
            .expect_err(text);
        let cq_err = compile_filter(FilterView::Spans, view_schema, text).expect_err(text);
        assert_eq!(
            cq_err.message, df_err.message,
            "message mismatch for `{text}`"
        );
        assert_eq!(
            cq_err.position.map(|p| (p.line, p.column)),
            df_err.position.map(|p| (p.line, p.column)),
            "position mismatch for `{text}`"
        );
    }

    // Scores view rejects score functions with exact message + position
    let (_scores_rb, scores_cb) = rt.block_on(collect_view_batch(&df_ctx, "scores"));
    let df_score_err = df_ctx
        .parse_filter(DfFilterView::Scores, "score('x') > 1")
        .expect_err("scores view score()");
    let cq_score_err = compile_filter(FilterView::Scores, scores_cb.schema(), "score('x') > 1")
        .expect_err("scores view score()");
    assert_eq!(cq_score_err.message, df_score_err.message);
    assert_eq!(
        cq_score_err.position.map(|p| (p.line, p.column)),
        df_score_err.position.map(|p| (p.line, p.column))
    );

    // 9th distinct score name rejected with exact message + position
    let nine_scores = (0..9)
        .map(|i| format!("score('s{i}') > 0"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let df_nine = df_ctx
        .parse_filter(DfFilterView::Spans, &nine_scores)
        .expect_err("9 scores");
    let cq_nine =
        compile_filter(FilterView::Spans, view_schema, &nine_scores).expect_err("9 scores");
    assert_eq!(cq_nine.message, df_nine.message);
    assert_eq!(
        cq_nine.position.map(|p| (p.line, p.column)),
        df_nine.position.map(|p| (p.line, p.column))
    );

    // Binding-phase type errors also reject on both engines
    let bind_error_cases = [
        "tags @> 123",
        "start_time + 1 = start_time",
        "duration_ns = true",
        "lower(duration_ns) = '1'",
    ];
    for text in bind_error_cases {
        let df_res = df_ctx.parse_filter(DfFilterView::Spans, text);
        let cq_res = compile_filter(FilterView::Spans, view_schema, text);
        assert!(df_res.is_err(), "expected df err for `{text}`");
        assert!(cq_res.is_err(), "expected cq err for `{text}`");
    }
}

#[test]
fn tombstone_predicate_matches_datafusion_across_all_tables() {
    let project = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap();
    let ctx = eval_ctx();

    type DeletionSpecInput<'a> = (i64, Vec<[u8; 16]>, Vec<&'a str>, Vec<&'a str>);
    let make_deletions = |specs: Vec<DeletionSpecInput<'_>>| {
        let mut df_dels = Vec::with_capacity(specs.len());
        let mut cq_dels = Vec::with_capacity(specs.len());
        for (idx, (cutoff, trace_ids, users, sessions)) in specs.into_iter().enumerate() {
            df_dels.push(Arc::new(PendingDeletion {
                id: Uuid::from_u128((idx + 1) as u128),
                seq: (idx + 1) as u64,
                project,
                cutoff,
                registered_at: 2,
                trace_ids: trace_ids.iter().copied().collect(),
                users: users.iter().map(|s| (*s).to_owned()).collect(),
                sessions: sessions.iter().map(|s| (*s).to_owned()).collect(),
            }));
            cq_dels.push(TombstoneSpec {
                cutoff,
                trace_ids,
                users: users.into_iter().map(str::to_owned).collect(),
                sessions: sessions.into_iter().map(str::to_owned).collect(),
            });
        }
        (df_dels, cq_dels)
    };

    let tid = |n: u16| *span_row(n, 0, 0, 0).trace_id.as_bytes();

    // 1. Spans stored batch: trace_id, user_id (nullable), session_id (ignored on spans), cutoff boundaries
    let mut s0 = span_row(1, 0, 10, 1_000);
    s0.user_id = Some("alice@secret.test".to_owned());
    let mut s1 = span_row(1, 1, 20, 1_001); // past cutoff -> kept
    s1.user_id = Some("alice@secret.test".to_owned());
    let mut s2 = span_row(2, 0, 30, 500);
    s2.user_id = Some("bob@secret.test".to_owned());
    let mut s3 = span_row(3, 0, 40, 500); // null user_id, session matches only on scores
    s3.session_id = Some("sess-erased".to_owned());
    let mut s4 = span_row(4, 0, 50, 999); // covered by trace_id in deletion 2
    s4.user_id = None;
    let spans_rb = span_rows(&[s0, s1, s2, s3, s4]);
    let spans_cb = record_batch_to_column(&spans_rb).unwrap();

    let (df_dels, cq_dels) = make_deletions(vec![
        (
            1_000,
            vec![tid(1)],
            vec!["bob@secret.test", "bob@secret.test"],
            vec!["sess-erased"],
        ),
        (1_000, vec![tid(4), tid(4)], vec![], vec![]),
    ]);

    assert_tombstone_parity(
        FileTable::Spans,
        TombstoneTable::Spans,
        &spans_schema(),
        &spans_rb,
        &spans_cb,
        &df_dels,
        &cq_dels,
        &ctx,
    );

    // 2. Scores stored batch: nullable trace_id (session scores), user_id, session_id, cutoff
    let sc0 = score_row(1, 1, 10, 10, 1_000, false, "q"); // trace_id tid(1), ingest 1000 -> covered
    let sc1 = score_row(2, 1, 10, 10, 1_001, false, "q"); // past cutoff -> kept
    let mut sc2 = score_row(3, 9, 10, 10, 500, false, "q");
    sc2.target = ScoreTarget::Session {
        session_id: "sess-erased".to_owned(),
    };
    sc2.session_id = Some("sess-erased".to_owned()); // session covered on scores -> covered
    let mut sc3 = score_row(4, 9, 10, 10, 500, false, "q");
    sc3.target = ScoreTarget::Session {
        session_id: "sess-kept".to_owned(),
    };
    sc3.session_id = Some("sess-kept".to_owned());
    sc3.user_id = None; // null trace_id, null user_id -> false, never NULL
    let mut sc4 = score_row(5, 9, 10, 10, 500, false, "q");
    sc4.user_id = Some("bob@secret.test".to_owned()); // user covered -> covered

    let scores_rb = score_rows(&[sc0, sc1, sc2, sc3, sc4]);
    let scores_cb = record_batch_to_column(&scores_rb).unwrap();
    assert_tombstone_parity(
        FileTable::Scores,
        TombstoneTable::Scores,
        &scores_schema(),
        &scores_rb,
        &scores_cb,
        &df_dels,
        &cq_dels,
        &ctx,
    );

    // 3. Traces stored batch: trace_id only (no ingest_time gate; user/session ignored)
    let t0 = span_row(1, 0, 10, 999_999); // ingest_time > cutoff still covered on traces!
    let t1 = span_row(2, 0, 20, 500); // only user matched in del 1 -> kept on traces!
    let t2 = span_row(4, 0, 30, 500); // covered by tid(4)
    let traces_rb = trace_batch(&[t0, t1, t2]);
    let traces_cb = record_batch_to_column(&traces_rb).unwrap();
    assert_tombstone_parity(
        FileTable::Traces,
        TombstoneTable::Traces,
        &traces_schema(),
        &traces_rb,
        &traces_cb,
        &df_dels,
        &cq_dels,
        &ctx,
    );

    // Empty/irrelevant deletions -> Ok(None) on both
    let (df_user_only, cq_user_only) = make_deletions(vec![(1_000, vec![], vec!["u"], vec!["s"])]);
    assert!(
        df_covered_predicate(FileTable::Traces, &traces_schema(), &df_user_only)
            .unwrap()
            .is_none()
    );
    assert!(
        cetra_query::covered_predicate(TombstoneTable::Traces, traces_cb.schema(), &cq_user_only)
            .unwrap()
            .is_none()
    );
    let _ = Reservation::size;
    let _ = ScalarValue::Null(DataType::Null);
    let _ = Field::new("x", DataType::Bool, true);
    let _ = Schema::try_new(vec![]);
    let _ = eval_expr;
}

#[allow(clippy::too_many_arguments)]
fn assert_tombstone_parity(
    df_table: FileTable,
    cq_table: TombstoneTable,
    arrow_schema: &Arc<ArrowSchema>,
    rb: &RecordBatch,
    cb: &Batch,
    df_dels: &[Arc<PendingDeletion>],
    cq_dels: &[TombstoneSpec],
    ctx: &EvalContext,
) {
    assert_eq!(
        cetra_query::key_columns(cq_table),
        crate::deletion::key_columns(df_table)
    );
    let df_pred = df_covered_predicate(df_table, arrow_schema, df_dels)
        .unwrap()
        .expect("df pred");
    let cq_pred = cetra_query::covered_predicate(cq_table, cb.schema(), cq_dels)
        .unwrap()
        .expect("cq pred");

    // Verify redaction parity on Display/Debug
    let df_plan = df_pred.to_string();
    let cq_plan = cq_pred.fmt_plan();
    if df_table != FileTable::Traces {
        assert!(df_plan.contains("user_id IN (<1 redacted>)"), "{df_plan}");
        assert!(cq_plan.contains("user_id IN (<1 redacted>)"), "{cq_plan}");
        assert!(!cq_plan.contains("bob@secret.test"), "{cq_plan}");
    }
    if df_table == FileTable::Scores {
        assert!(
            df_plan.contains("session_id IN (<1 redacted>)"),
            "{df_plan}"
        );
        assert!(
            cq_plan.contains("session_id IN (<1 redacted>)"),
            "{cq_plan}"
        );
        assert!(!cq_plan.contains("sess-erased"), "{cq_plan}");
    }

    // Row-for-row covered mask parity
    let df_vals = df_pred
        .evaluate(rb)
        .unwrap()
        .into_array(rb.num_rows())
        .unwrap();
    let df_bools: &BooleanArray = df_vals.as_boolean_opt().unwrap();
    let cq_bool_col = eval_expr(&cq_pred, cb, ctx, None).unwrap();
    let cq_mask = eval_filter(&cq_pred, cb, ctx, None).unwrap();
    for row in 0..rb.num_rows() {
        assert!(
            !df_bools.is_null(row),
            "df predicate produced NULL at row {row}"
        );
        assert!(
            !cq_bool_col.is_null(row),
            "cq predicate produced NULL at row {row}"
        );
        let df_cov = df_bools.value(row);
        let cq_cov = cq_mask.get(row);
        assert_eq!(cq_cov, df_cov, "row {row} covered mismatch on {cq_table:?}");
    }

    // drop_covered vs filter_uncovered parity
    let df_kept = df_drop_covered(rb, &df_pred).unwrap();
    let cq_kept = cetra_query::filter_uncovered(cb, &cq_pred, ctx, None).unwrap();
    assert_eq!(
        cq_kept.num_rows(),
        df_kept.num_rows(),
        "{cq_table:?} kept count"
    );

    let cq_indices = cq_kept.selection().to_indices(cq_kept.raw_len()).unwrap();
    let df_tids = df_kept.column_by_name("trace_id").unwrap();
    let cq_tids = cq_kept
        .column(cb.schema().index_of("trace_id").unwrap())
        .unwrap();
    for (out_i, &orig_row) in cq_indices.iter().enumerate() {
        let df_is_null = df_tids.is_null(out_i);
        let cq_is_null = cq_tids.is_null(orig_row as usize);
        assert_eq!(cq_is_null, df_is_null);
        if !df_is_null {
            let df_bytes = df_tids
                .as_any()
                .downcast_ref::<arrow::array::FixedSizeBinaryArray>()
                .unwrap()
                .value(out_i);
            let ScalarValue::FixedBytes16(cq_bytes) =
                cq_tids.get_scalar(orig_row as usize).unwrap()
            else {
                panic!("expected FixedBytes16");
            };
            assert_eq!(cq_bytes.as_slice(), df_bytes);
        }
    }
}
