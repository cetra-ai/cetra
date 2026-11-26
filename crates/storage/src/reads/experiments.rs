//! Native query engine implementations of `experiment_scores`, `experiment_traces`,
//! and `experiment_score_means` (D-J-13, D-J-26).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use cetra_column::{DataType, ScalarValue, Schema};
use cetra_core::enums::ScoreDataType;
use cetra_core::names::metadata_key;
use cetra_core::schema::{scores, traces};
use cetra_query::exec::hash_agg::execute_hash_agg;
use cetra_query::exec::{
    AggExpr, AggFunc, CollectSink, HashAggSpec, MapProjection, ScanSource, ScanSpec, TimeRangeHint,
    run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, Expr, QueryError};

use crate::query_error::RunError;
use crate::reads::columns::{
    active_rows, raw_f64_at, raw_i64_at, raw_string_at, raw_time_at, require_col,
};
use crate::reads::context::{
    query_err_to_run, run_err_to_query, scores_raw_schema, traces_raw_schema,
};
use crate::reads::targets::raw_fixed16_at;
use crate::views::QueryContext;

/// Most score rows [`experiment_scores`] returns (CG-J2).
pub const MAX_EXPERIMENT_SCORES: usize = 500_000;
/// Most trace rows [`experiment_traces`] returns.
pub const MAX_EXPERIMENT_TRACES: usize = 100_000;
/// Most experiments [`experiment_score_means`] takes in one read (D-J-26).
pub const MAX_MEAN_EXPERIMENTS: usize = 1_000;

/// One live score of an experiment.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentScore {
    /// Lowercase hex.
    pub trace_id: String,
    /// The dataset item (stable id) the score is about, hyphenated.
    pub dataset_item_id: Option<String>,
    /// Score name.
    pub name: String,
    /// `numeric`, `boolean`, `categorical` or `text`.
    pub data_type: String,
    /// The numeric value (or the `categorical` mapping).
    pub value_num: Option<f64>,
    /// The label of a `categorical` score, or the text of a `text` score.
    pub value_str: Option<String>,
}

/// The mean of one score name and data type over the live scores of one experiment (D-J-26).
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentScoreMean {
    /// Lowercase hyphenated UUID, as the experiment was asked for.
    pub experiment_id: String,
    /// Score name.
    pub name: String,
    /// `numeric` or `boolean`.
    pub data_type: String,
    /// Mean of the numeric values over the score rows; `None` when none has a value.
    pub mean: Option<f64>,
    /// Score rows counted.
    pub count: i64,
}

/// One trace of an experiment.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentTrace {
    /// Lowercase hex.
    pub trace_id: String,
    /// The merged trace cost, when any span has one.
    pub cost_total: Option<f64>,
    /// The trace's end minus its start, in nanoseconds.
    pub duration_ns: i64,
    /// Spans of the trace with an error status.
    pub error_count: i64,
}

const MEAN: &str = "mean";
const COUNT: &str = "count";

/// Live, non-deleted scores whose `experiment_id` is `experiment` created at or after `since`.
pub async fn experiment_scores(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
    timeout: Duration,
) -> Result<Vec<ExperimentScore>, RunError> {
    let ctx = ctx.engine();
    let experiment = experiment_key(experiment)?;
    let scores_raw = scores_raw_schema()?;

    let mut min_ns = since;
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }

    let created_col = col_expr(&scores_raw, scores::CREATED_AT).map_err(query_err_to_run)?;
    let mut filters = vec![
        Expr::Binary {
            left: Box::new(col_expr(&scores_raw, scores::EXPERIMENT_ID).map_err(query_err_to_run)?),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(experiment))),
            data_type: DataType::Bool,
        },
        Expr::Binary {
            left: Box::new(col_expr(&scores_raw, scores::DELETED).map_err(query_err_to_run)?),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Bool(false))),
            data_type: DataType::Bool,
        },
        Expr::Binary {
            left: Box::new(created_col.clone()),
            op: BinaryOp::GtEq,
            right: Box::new(ts_lit(since)),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(Expr::Binary {
            left: Box::new(created_col),
            op: BinaryOp::GtEq,
            right: Box::new(ts_lit(vf)),
            data_type: DataType::Bool,
        });
    }

    let needed = [
        scores::TRACE_ID,
        scores::DATASET_ITEM_ID,
        scores::NAME,
        scores::DATA_TYPE,
        scores::VALUE_NUM,
        scores::VALUE_STR,
    ];
    let mut proj_indices = Vec::with_capacity(needed.len());
    for name in needed {
        let idx = scores_raw
            .index_of(name)
            .ok_or_else(|| QueryError::internal(format!("missing {name} in scores schema")))
            .map_err(query_err_to_run)?;
        proj_indices.push(idx);
    }

    let spec = ScanSpec {
        projection: Some(proj_indices),
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns: None,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: Some(MAX_EXPERIMENT_SCORES + 1),
    };

    let ctx_c = ctx.clone();
    let (found, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let streams = ctx_c.scores.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(Vec::new());
            }
            let pool_c = Arc::clone(&exec_ctx.pool);
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let mut found = Vec::new();
            let mut seen = 0usize;
            for sink in sinks {
                for batch in sink.into_batches() {
                    seen += batch.num_rows();
                    // Check cap BEFORE skipping rows with `trace_id IS NULL`.
                    if seen > MAX_EXPERIMENT_SCORES {
                        return Err(QueryError::InvalidRequest(format!(
                            "experiment has more than {MAX_EXPERIMENT_SCORES} scores"
                        )));
                    }
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    let trace_col =
                        require_col(&batch, scores::TRACE_ID).map_err(run_err_to_query)?;
                    let item_col =
                        require_col(&batch, scores::DATASET_ITEM_ID).map_err(run_err_to_query)?;
                    let name_col = require_col(&batch, scores::NAME).map_err(run_err_to_query)?;
                    let dt_col =
                        require_col(&batch, scores::DATA_TYPE).map_err(run_err_to_query)?;
                    let num_col =
                        require_col(&batch, scores::VALUE_NUM).map_err(run_err_to_query)?;
                    let str_col =
                        require_col(&batch, scores::VALUE_STR).map_err(run_err_to_query)?;

                    for phys in active_rows(&batch) {
                        let Some(trace_bytes) = raw_fixed16_at(trace_col, phys) else {
                            continue;
                        };
                        found.push(ExperimentScore {
                            trace_id: hex::encode(trace_bytes),
                            dataset_item_id: raw_string_at(item_col, phys)
                                .map_err(run_err_to_query)?,
                            name: raw_string_at(name_col, phys)
                                .map_err(run_err_to_query)?
                                .ok_or_else(bad_row)?,
                            data_type: raw_string_at(dt_col, phys)
                                .map_err(run_err_to_query)?
                                .ok_or_else(bad_row)?,
                            value_num: raw_f64_at(num_col, phys).map_err(run_err_to_query)?,
                            value_str: raw_string_at(str_col, phys).map_err(run_err_to_query)?,
                        });
                    }
                }
            }
            Ok(found)
        })
        .await?;

    Ok(found)
}

/// Traces whose merged `metadata['cetra.experiment_id']` is `experiment`, starting at or after `since`.
/// Default exclusions do not apply.
pub async fn experiment_traces(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
    timeout: Duration,
) -> Result<Vec<ExperimentTrace>, RunError> {
    let ctx = ctx.engine();
    let experiment = experiment_key(experiment)?;
    let traces_raw = traces_raw_schema()?;

    let mut min_ns = since;
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }

    let meta_col = col_expr(&traces_raw, traces::METADATA).map_err(query_err_to_run)?;
    let start_col = col_expr(&traces_raw, traces::START_TIME).map_err(query_err_to_run)?;
    let mut filters = vec![
        Expr::Binary {
            left: Box::new(Expr::MapIndex {
                map: Box::new(meta_col),
                key: Box::new(Expr::Literal(ScalarValue::Utf8(
                    metadata_key::EXPERIMENT_ID.to_owned(),
                ))),
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(experiment))),
            data_type: DataType::Bool,
        },
        Expr::Binary {
            left: Box::new(start_col.clone()),
            op: BinaryOp::GtEq,
            right: Box::new(ts_lit(since)),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(Expr::Binary {
            left: Box::new(start_col),
            op: BinaryOp::GtEq,
            right: Box::new(ts_lit(vf)),
            data_type: DataType::Bool,
        });
    }

    let needed = [
        traces::TRACE_ID,
        traces::COST_TOTAL,
        traces::START_TIME,
        traces::END_TIME,
        traces::ERROR_COUNT,
    ];
    let mut proj_indices = Vec::with_capacity(needed.len());
    for name in needed {
        let idx = traces_raw
            .index_of(name)
            .ok_or_else(|| QueryError::internal(format!("missing {name} in traces schema")))
            .map_err(query_err_to_run)?;
        proj_indices.push(idx);
    }

    let mut map_projections = BTreeMap::new();
    map_projections.insert(
        traces::METADATA.to_owned(),
        MapProjection::Keys(BTreeSet::from([metadata_key::EXPERIMENT_ID.to_owned()])),
    );
    let spec = ScanSpec {
        projection: Some(proj_indices),
        map_projections,
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns: None,
            around_ns: ctx.around,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: Some(MAX_EXPERIMENT_TRACES + 1),
    };

    let ctx_c = ctx.clone();
    let (found, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(Vec::new());
            }
            let pool_c = Arc::clone(&exec_ctx.pool);
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let mut found = Vec::new();
            let mut seen = 0usize;
            for sink in sinks {
                for batch in sink.into_batches() {
                    seen += batch.num_rows();
                    if seen > MAX_EXPERIMENT_TRACES {
                        return Err(QueryError::InvalidRequest(format!(
                            "experiment has more than {MAX_EXPERIMENT_TRACES} traces"
                        )));
                    }
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    let trace_col =
                        require_col(&batch, traces::TRACE_ID).map_err(run_err_to_query)?;
                    let cost_col =
                        require_col(&batch, traces::COST_TOTAL).map_err(run_err_to_query)?;
                    let start_col =
                        require_col(&batch, traces::START_TIME).map_err(run_err_to_query)?;
                    let end_col =
                        require_col(&batch, traces::END_TIME).map_err(run_err_to_query)?;
                    let err_col =
                        require_col(&batch, traces::ERROR_COUNT).map_err(run_err_to_query)?;

                    for phys in active_rows(&batch) {
                        let Some(trace_bytes) = raw_fixed16_at(trace_col, phys) else {
                            continue;
                        };
                        let start = raw_time_at(start_col, phys).map_err(run_err_to_query)?;
                        let end = raw_time_at(end_col, phys).map_err(run_err_to_query)?;
                        found.push(ExperimentTrace {
                            trace_id: hex::encode(trace_bytes),
                            cost_total: raw_f64_at(cost_col, phys).map_err(run_err_to_query)?,
                            duration_ns: end.saturating_sub(start),
                            error_count: raw_i64_at(err_col, phys).map_err(run_err_to_query)?,
                        });
                    }
                }
            }
            Ok(found)
        })
        .await?;

    Ok(found)
}

/// Per experiment, score name and data type (`numeric` and `boolean`), the mean and row count
/// of live scores created at or after `since`.
pub async fn experiment_score_means(
    ctx: &QueryContext,
    experiments: &[String],
    since: i64,
    timeout: Duration,
) -> Result<Vec<ExperimentScoreMean>, RunError> {
    let ctx = ctx.engine();
    if experiments.len() > MAX_MEAN_EXPERIMENTS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_MEAN_EXPERIMENTS} experiments"
        )));
    }
    let parsed: Vec<String> = experiments
        .iter()
        .map(|e| experiment_key(e))
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        return Ok(Vec::new());
    }
    let distinct: Vec<String> = parsed
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let scores_raw = scores_raw_schema()?;
    let mut min_ns = since;
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }

    let created_col = col_expr(&scores_raw, scores::CREATED_AT).map_err(query_err_to_run)?;
    let mut filters = vec![
        Expr::InList {
            expr: Box::new(col_expr(&scores_raw, scores::EXPERIMENT_ID).map_err(query_err_to_run)?),
            list: distinct
                .iter()
                .map(|id| Expr::Literal(ScalarValue::Utf8(id.clone())))
                .collect(),
            negated: false,
            redacted: false,
        },
        Expr::Binary {
            left: Box::new(col_expr(&scores_raw, scores::DELETED).map_err(query_err_to_run)?),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Bool(false))),
            data_type: DataType::Bool,
        },
        Expr::Binary {
            left: Box::new(created_col.clone()),
            op: BinaryOp::GtEq,
            right: Box::new(ts_lit(since)),
            data_type: DataType::Bool,
        },
        Expr::InList {
            expr: Box::new(col_expr(&scores_raw, scores::DATA_TYPE).map_err(query_err_to_run)?),
            list: vec![
                Expr::Literal(ScalarValue::Utf8(
                    ScoreDataType::Numeric.as_str().to_owned(),
                )),
                Expr::Literal(ScalarValue::Utf8(
                    ScoreDataType::Boolean.as_str().to_owned(),
                )),
            ],
            negated: false,
            redacted: false,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(Expr::Binary {
            left: Box::new(created_col),
            op: BinaryOp::GtEq,
            right: Box::new(ts_lit(vf)),
            data_type: DataType::Bool,
        });
    }

    let needed = [
        scores::EXPERIMENT_ID,
        scores::NAME,
        scores::DATA_TYPE,
        scores::VALUE_NUM,
    ];
    let mut proj_indices = Vec::with_capacity(needed.len());
    for name in needed {
        let idx = scores_raw
            .index_of(name)
            .ok_or_else(|| QueryError::internal(format!("missing {name} in scores schema")))
            .map_err(query_err_to_run)?;
        proj_indices.push(idx);
    }

    let spec = ScanSpec {
        projection: Some(proj_indices),
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns: None,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let ctx_c = ctx.clone();
    let (found, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let proj_schema = ctx_c.scores.projected_schema(&spec)?;
            let agg_spec = HashAggSpec {
                group_by: vec![
                    (
                        scores::EXPERIMENT_ID.to_owned(),
                        col_expr(&proj_schema, scores::EXPERIMENT_ID)?,
                    ),
                    (
                        scores::NAME.to_owned(),
                        col_expr(&proj_schema, scores::NAME)?,
                    ),
                    (
                        scores::DATA_TYPE.to_owned(),
                        col_expr(&proj_schema, scores::DATA_TYPE)?,
                    ),
                ],
                aggs: vec![
                    AggExpr {
                        func: AggFunc::Avg(col_expr(&proj_schema, scores::VALUE_NUM)?),
                        filter: None,
                        output_name: MEAN.to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
                        filter: None,
                        output_name: COUNT.to_owned(),
                    },
                ],
            };

            let batches = execute_hash_agg(ctx_c.scores.as_ref(), &spec, agg_spec, exec_ctx)?;
            let mut found = Vec::new();
            for batch in &batches {
                if batch.num_rows() == 0 {
                    continue;
                }
                let exp_col =
                    require_col(batch, scores::EXPERIMENT_ID).map_err(run_err_to_query)?;
                let name_col = require_col(batch, scores::NAME).map_err(run_err_to_query)?;
                let dt_col = require_col(batch, scores::DATA_TYPE).map_err(run_err_to_query)?;
                let mean_col = require_col(batch, MEAN).map_err(run_err_to_query)?;
                let count_col = require_col(batch, COUNT).map_err(run_err_to_query)?;

                for phys in active_rows(batch) {
                    found.push(ExperimentScoreMean {
                        experiment_id: raw_string_at(exp_col, phys)
                            .map_err(run_err_to_query)?
                            .ok_or_else(bad_row)?,
                        name: raw_string_at(name_col, phys)
                            .map_err(run_err_to_query)?
                            .ok_or_else(bad_row)?,
                        data_type: raw_string_at(dt_col, phys)
                            .map_err(run_err_to_query)?
                            .ok_or_else(bad_row)?,
                        mean: raw_f64_at(mean_col, phys).map_err(run_err_to_query)?,
                        count: raw_i64_at(count_col, phys).map_err(run_err_to_query)?,
                    });
                }
            }
            Ok(found)
        })
        .await?;

    Ok(found)
}

fn experiment_key(experiment: &str) -> Result<String, RunError> {
    uuid::Uuid::parse_str(experiment)
        .map(|id| id.to_string())
        .map_err(|_| RunError::InvalidRequest("invalid experiment id".to_owned()))
}

fn ts_lit(ns: i64) -> Expr {
    Expr::Literal(ScalarValue::TimestampNs {
        value: ns,
        utc: true,
    })
}

fn col_expr(schema: &Schema, name: &str) -> Result<Expr, QueryError> {
    let idx = schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("missing column {name} in schema")))?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

fn bad_row() -> QueryError {
    QueryError::internal("a view returned a null where a value is required")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::SpanStatus;
    use cetra_core::ids::ProjectId;
    use cetra_core::names::metadata_key;
    use cetra_core::score::{Score, ScoreValue};
    use cetra_core::span::Span;

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::test_support::{score_row, score_rows, span_row, span_rows};
    use crate::views::QueryContext;

    const TIMEOUT: Duration = Duration::from_secs(30);
    const E1: &str = "01900000-0000-7000-8000-000000000e01";
    const E2: &str = "01900000-0000-7000-8000-000000000e02";
    const ITEM: &str = "01900000-0000-7000-8000-000000000d01";

    fn test_config(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            data_dir: dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig {
                target_partitions: 2,
                ..QueryConfig::default()
            },
            anchor: Default::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        }
    }

    #[tokio::test]
    async fn experiments_all_3_functions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut s0a = span_row(0, 0, 1_000, 0);
        s0a.metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), E1.to_owned());
        s0a.cost_total = Some(0.25);
        let mut s0b = span_row(0, 1, 1_050, 0);
        s0b.parent_span_id = Some(s0a.span_id);
        s0b.status = SpanStatus::Error;
        let mut s1 = span_row(1, 0, 2_000, 0);
        s1.metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), E2.to_owned());
        let plain = span_row(2, 0, 3_000, 0);
        let spans: Vec<Span> = vec![s0a, s0b, s1, plain];

        let mut sc1 = score_row(1, 0, 100, 100, 100, false, "accuracy");
        sc1.experiment_id = Some(E1.parse().expect("e1"));
        sc1.dataset_item_id = Some(ITEM.parse().expect("item"));
        sc1.value = ScoreValue::Numeric(0.5);

        let mut sc2 = score_row(2, 0, 150, 150, 150, false, "accuracy");
        sc2.experiment_id = Some(E1.parse().expect("e1"));
        sc2.value = ScoreValue::Numeric(1.5);

        let mut sc_bool = score_row(3, 0, 160, 160, 160, false, "passed");
        sc_bool.experiment_id = Some(E1.parse().expect("e1"));
        sc_bool.value = ScoreValue::Boolean(true);

        let mut sc_cat = score_row(4, 0, 170, 170, 170, false, "tone");
        sc_cat.experiment_id = Some(E1.parse().expect("e1"));
        sc_cat.value = ScoreValue::Categorical {
            label: "warm".to_owned(),
            value: None,
        };

        // Deleted score (replaced) must not be returned
        let mut sc_del_old = score_row(5, 0, 180, 100, 100, false, "gone");
        sc_del_old.experiment_id = Some(E1.parse().expect("e1"));
        let mut sc_del_new = sc_del_old.clone();
        sc_del_new.updated_at = 200;
        sc_del_new.deleted = true;

        let mut sc_e2 = score_row(6, 1, 200, 200, 200, false, "accuracy");
        sc_e2.experiment_id = Some(E2.parse().expect("e2"));
        sc_e2.value = ScoreValue::Numeric(0.25);

        // Session score (trace_id is None): ignored by experiment_scores output!
        let mut sc_sess = score_row(7, 0, 190, 190, 190, false, "sess_score");
        sc_sess.experiment_id = Some(E1.parse().expect("e1"));
        sc_sess.target = cetra_core::score::ScoreTarget::Session {
            session_id: "s1".to_owned(),
        };

        let scores: Vec<Score> = vec![
            sc1, sc2, sc_bool, sc_cat, sc_del_old, sc_del_new, sc_e2, sc_sess,
        ];

        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("spans");
        storage
            .append_scores(project, score_rows(&scores))
            .await
            .expect("scores");

        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let ctx = QueryContext::new(pq, None).expect("ctx");

        // 1. experiment_scores
        for (since, expected_len) in [(0, 4), (120, 3), (500, 0)] {
            let mut q = experiment_scores(&ctx, E1, since, TIMEOUT)
                .await
                .expect("experiment_scores");
            q.sort_by(|a, b| (&a.name, &a.trace_id).cmp(&(&b.name, &b.trace_id)));
            assert_eq!(q.len(), expected_len);
            if since == 0 {
                assert_eq!(q[0].dataset_item_id.as_deref(), Some(ITEM));
            }
        }

        // 2. experiment_traces
        let t0 = experiment_traces(&ctx, E1, 0, TIMEOUT)
            .await
            .expect("experiment_traces");
        assert_eq!(t0.len(), 1);
        assert_eq!(t0[0].cost_total, Some(0.25));
        assert_eq!(t0[0].error_count, 1);
        assert!(t0[0].duration_ns > 0);
        assert_eq!(
            experiment_traces(&ctx, E1, 1_000, TIMEOUT)
                .await
                .expect("since 1000"),
            t0
        );
        assert!(
            experiment_traces(&ctx, E1, 1_001, TIMEOUT)
                .await
                .expect("since 1001")
                .is_empty()
        );

        // 3. experiment_score_means
        let asked = [E1.to_owned(), E2.to_owned()];
        let mut m0 = experiment_score_means(&ctx, &asked, 0, TIMEOUT)
            .await
            .expect("means 0");
        m0.sort_by(|a, b| {
            (&a.experiment_id, &a.name, &a.data_type).cmp(&(
                &b.experiment_id,
                &b.name,
                &b.data_type,
            ))
        });
        assert_eq!(m0.len(), 4);
        assert_eq!(m0[0].name, "accuracy");
        assert_eq!(m0[0].count, 2);
        assert_eq!(m0[0].mean, Some(1.0));
        assert_eq!(m0[1].name, "passed");
        assert_eq!(m0[1].count, 1);
        assert_eq!(m0[1].mean, Some(1.0));
        assert_eq!(m0[2].name, "sess_score");
        assert_eq!(m0[2].count, 1);
        assert_eq!(m0[2].mean, Some(0.5));
        assert_eq!(m0[3].experiment_id, E2);
        assert_eq!(m0[3].mean, Some(0.25));

        let mut m120 = experiment_score_means(&ctx, &asked, 120, TIMEOUT)
            .await
            .expect("means 120");
        m120.sort_by(|a, b| {
            (&a.experiment_id, &a.name, &a.data_type).cmp(&(
                &b.experiment_id,
                &b.name,
                &b.data_type,
            ))
        });
        assert_eq!(m120[0].count, 1);
        assert_eq!(m120[0].mean, Some(1.5));

        // Validation errors
        assert!(matches!(
            experiment_scores(&ctx, "bad-uuid", 0, TIMEOUT).await,
            Err(RunError::InvalidRequest(ref m)) if m == "invalid experiment id"
        ));
        assert!(matches!(
            experiment_traces(&ctx, "bad-uuid", 0, TIMEOUT).await,
            Err(RunError::InvalidRequest(ref m)) if m == "invalid experiment id"
        ));
        let too_many = vec![E1.to_owned(); MAX_MEAN_EXPERIMENTS + 1];
        assert!(matches!(
            experiment_score_means(&ctx, &too_many, 0, TIMEOUT).await,
            Err(RunError::InvalidRequest(_))
        ));

        let _ = storage.shutdown().await;
    }
}
