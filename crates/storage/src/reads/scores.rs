//! Native query-engine (`cetra-query` + `cetra-column`) implementations of the
//! `scores` read entry points: `score_list`, `latest_scores`, `scores_by_id`,
//! and `score_name_counts`.

use std::sync::Arc;
use std::time::Duration;

use cetra_column::{Batch, DataType, Field, ScalarValue, Schema};
use cetra_core::enums::{ScoreSource, ScoreTargetKind};
use cetra_core::schema::scores;
use cetra_query::exec::sort_window::{WindowFunc, WindowSpec, eval_window_group};
use cetra_query::exec::streaming::{FilterOp, ProjectOp, TopKSink};
use cetra_query::exec::{
    AggExpr, AggFunc, CollectSink, ControlFlow, ExecContext, HashAgg, HashAggSpec, PartitionStream,
    PushOperator, ScanSource, ScanSpec, SortKey, TimeRangeHint, run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, CastMode, Expr, QueryError, ScalarFn};

use crate::query_error::RunError;
use crate::reads::columns::{i64_at, internal, require_col, string_at};
use crate::reads::context::{
    OperatorStream, query_err_to_run, scores_raw_schema, scores_view_projection,
};
use crate::reads::cursor::{self, Page};
use crate::reads::{MAX_PAGE, SortOrder};
use crate::views::{QueryContext, TimeWindow};

const LIST: &str = "scores";
const TARGET_KEY: &str = "target_key";
const RANK: &str = "__rn";
const TIES: &[&str] = &[scores::SCORE_ID];
const BUCKET: &str = "bucket";
const COUNT: &str = "count";
const USERS: &str = "users";
const LAST: &str = "last_created_at";

/// Most names one [`score_name_counts`] takes (D-IC-9).
pub const MAX_COUNT_NAMES: usize = 1_000;

/// Live scores of one name, in one UTC day or in the whole window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreNameCount {
    /// Score name.
    pub name: String,
    /// Start of the bucket (ns), or `window.from` when `bucket_ns` is `None`.
    pub bucket_start: i64,
    /// Live scores.
    pub count: u64,
    /// Distinct non-null `user_id`s.
    pub users: u64,
    /// Greatest `created_at` among them (ns).
    pub last_created_at: i64,
}

/// Names and window of one [`score_name_counts`].
#[derive(Debug, Clone, Copy)]
pub struct ScoreNameCountsRequest<'a> {
    /// Score names; empty returns no rows. At most [`MAX_COUNT_NAMES`].
    pub names: &'a [String],
    /// `created_at ∈ [from, to)`.
    pub window: TimeWindow,
    /// `Some(width)`: one row per (name, bucket), each bucket `[k·width, (k+1)·width)` ns since
    /// the epoch (`86_400_000_000_000` gives UTC days); `None`: one row per name for the whole
    /// window. A width must be positive.
    pub bucket_ns: Option<i64>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Columns of [`latest_scores`], in order.
pub const LATEST_SCORE_COLUMNS: &[&str] = &[
    TARGET_KEY,
    scores::NAME,
    scores::SCORE_ID,
    scores::DATA_TYPE,
    scores::VALUE_NUM,
    scores::VALUE_STR,
    scores::SOURCE,
];

/// One page of the scores view.
///
/// Every set field is an equality conjunct. `window` applies to `created_at`.
#[derive(Debug, Clone)]
pub struct ScoreListRequest<'a> {
    /// `target` column. Compared with [`ScoreTargetKind::as_str`].
    pub target: Option<ScoreTargetKind>,
    /// `trace_id`. Uppercase hex is accepted.
    pub trace_id: Option<&'a str>,
    /// `span_id`. Uppercase hex is accepted.
    pub span_id: Option<&'a str>,
    /// `session_id`.
    pub session_id: Option<&'a str>,
    /// `name`.
    pub name: Option<&'a str>,
    /// `source` column. Compared with [`ScoreSource::as_str`].
    pub source: Option<ScoreSource>,
    /// `evaluator_id` column, compared with the hyphenated lowercase UUID text.
    pub evaluator_id: Option<&'a str>,
    /// `rule_id` column, compared with the hyphenated lowercase UUID text.
    pub rule_id: Option<&'a str>,
    /// Only scores written from this annotation queue (D-AQ-6).
    pub queue_id: Option<&'a str>,
    /// `created_at ∈ [from, to)`.
    pub window: Option<TimeWindow>,
    /// Rows to return, from 1 to [`super::MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Trace or session ids for [`latest_scores`].
#[derive(Debug, Clone, Copy)]
pub enum ScoreTargets<'a> {
    /// Latest scores whose `target` is a trace.
    Traces(&'a [&'a str]),
    /// Latest scores whose `target` is a session.
    Sessions(&'a [&'a str]),
}

/// One page of scores, ordered `created_at DESC NULLS LAST, score_id DESC`.
pub async fn score_list(
    ctx: &QueryContext,
    request: ScoreListRequest<'_>,
) -> Result<Page, RunError> {
    let ctx = ctx.engine();
    if !(1..=MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    let cursor = match request.cursor {
        Some(text) => {
            let decoded = cursor::decode(text, LIST, scores::CREATED_AT, SortOrder::Desc)?;
            if decoded.tie_len() != 1 {
                return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
            }
            Some(decoded)
        }
        None => None,
    };

    let raw_schema = scores_raw_schema()?;
    let mut filters = list_filters(&raw_schema, &request, ctx.visible_from)?;
    if let Some(cur) = &cursor {
        filters.push(cursor::after_expr(
            cur,
            &raw_schema,
            scores::CREATED_AT,
            TIES,
        )?);
    }

    let mut min_ns = ctx.visible_from;
    let mut max_ns = None;
    if let Some(window) = request.window {
        min_ns = Some(min_ns.map_or(window.from(), |vf| vf.max(window.from())));
        max_ns = window.to().checked_sub(1);
    }

    let spec = ScanSpec {
        projection: None,
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns,
            max_ns,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let sort_keys = cursor::order_by(&raw_schema, scores::CREATED_AT, TIES, SortOrder::Desc)?;
    let fetch = request.limit + 1;
    let scores_source = Arc::clone(&ctx.scores);
    let raw_schema_c = Arc::clone(&raw_schema);
    let sort_keys_c = sort_keys.clone();

    let (view_batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let partitions = scores_source.open_partitions(&spec, exec_ctx)?;
            if partitions.is_empty() {
                return Ok(Vec::new());
            }
            let sink_schema = Arc::clone(&raw_schema_c);
            let sink_keys = sort_keys_c.clone();
            let sink_ctx = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                partitions,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&sink_schema),
                        sink_keys.clone(),
                        fetch,
                        0,
                        sink_ctx.clone(),
                    ))
                },
            )?;
            let raw_batches = TopKSink::merge(
                sinks,
                Arc::clone(&raw_schema_c),
                &sort_keys_c,
                0,
                fetch,
                exec_ctx,
            )?;
            project_batches_to_scores_view(&raw_schema_c, raw_batches, exec_ctx)
        })
        .await?;

    cursor::assemble(
        view_batches,
        request.limit,
        LIST,
        scores::CREATED_AT,
        SortOrder::Desc,
        TIES,
    )
}

/// Latest live score of each name for the given traces or sessions (excluding `source = 'issue'`).
pub async fn latest_scores(
    ctx: &QueryContext,
    targets: ScoreTargets<'_>,
    timeout: Duration,
) -> Result<Vec<Batch>, RunError> {
    let ctx = ctx.engine();
    let Some(prepared) = prepare_keys(targets)? else {
        return Ok(Vec::new());
    };
    let raw_schema = scores_raw_schema()?;
    let proj_cols = [
        prepared.column,
        scores::NAME,
        scores::SCORE_ID,
        scores::DATA_TYPE,
        scores::VALUE_NUM,
        scores::VALUE_STR,
        scores::SOURCE,
        scores::UPDATED_AT,
    ];
    let proj_indices: Vec<usize> = proj_cols
        .iter()
        .map(|name| {
            raw_schema
                .index_of(name)
                .ok_or_else(|| internal(format!("scores schema missing column {name}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut filters = vec![
        eq_lit_str(&raw_schema, scores::TARGET, prepared.kind.as_str())
            .map_err(query_err_to_run)?,
        in_list_expr(col_expr(&raw_schema, prepared.column)?, prepared.literals),
        Expr::Binary {
            left: Box::new(col_expr(&raw_schema, scores::SOURCE)?),
            op: BinaryOp::NotEq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(
                ScoreSource::Issue.as_str().to_owned(),
            ))),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(cmp_ts(
            &col_expr(&raw_schema, scores::CREATED_AT)?,
            BinaryOp::GtEq,
            vf,
        ));
    }

    let spec = ScanSpec {
        projection: Some(proj_indices),
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let scores_source = Arc::clone(&ctx.scores);
    let key_col_name = prepared.column;
    let (batches, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let scan_schema = scores_source.projected_schema(&spec)?;
            let partitions = scores_source.open_partitions(&spec, exec_ctx)?;
            if partitions.is_empty() {
                return Ok(Vec::new());
            }

            let pre_fields = vec![
                Field::new(TARGET_KEY, DataType::Utf8, true),
                Field::new(scores::NAME, DataType::Utf8, false),
                Field::new(scores::SCORE_ID, DataType::Utf8, false),
                Field::new(scores::DATA_TYPE, DataType::Utf8, false),
                Field::new(scores::VALUE_NUM, DataType::F64, true),
                Field::new(scores::VALUE_STR, DataType::Utf8, true),
                Field::new(scores::SOURCE, DataType::Utf8, false),
                Field::new(scores::UPDATED_AT, DataType::timestamp_utc(), false),
            ];
            let pre_schema = Schema::try_new_arc(pre_fields)?;
            let key_in = col_expr_q(&scan_schema, key_col_name)?;
            let target_key_expr = if key_in.data_type() == DataType::FixedBytes16 {
                Expr::Call {
                    func: ScalarFn::Encode,
                    args: vec![key_in, Expr::Literal(ScalarValue::Utf8("hex".to_owned()))],
                    data_type: DataType::Utf8,
                }
            } else {
                key_in
            };
            let pre_exprs = vec![
                target_key_expr,
                col_expr_q(&scan_schema, scores::NAME)?,
                col_expr_q(&scan_schema, scores::SCORE_ID)?,
                col_expr_q(&scan_schema, scores::DATA_TYPE)?,
                col_expr_q(&scan_schema, scores::VALUE_NUM)?,
                col_expr_q(&scan_schema, scores::VALUE_STR)?,
                col_expr_q(&scan_schema, scores::SOURCE)?,
                col_expr_q(&scan_schema, scores::UPDATED_AT)?,
            ];
            let projected_streams: Vec<Box<dyn PartitionStream>> = partitions
                .into_iter()
                .map(|s| {
                    let op = ProjectOp::with_schema(
                        pre_exprs.clone(),
                        Arc::clone(&pre_schema),
                        exec_ctx.clone(),
                    )?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(op)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()?;

            let chained = chain_streams(projected_streams);
            let partition_by = vec![
                col_expr_q(&pre_schema, TARGET_KEY)?,
                col_expr_q(&pre_schema, scores::NAME)?,
            ];
            let order_by = vec![
                SortKey {
                    expr: col_expr_q(&pre_schema, scores::UPDATED_AT)?,
                    descending: true,
                    nulls_first: true,
                },
                SortKey {
                    expr: col_expr_q(&pre_schema, scores::SCORE_ID)?,
                    descending: true,
                    nulls_first: true,
                },
            ];
            let win_spec = WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: RANK.to_owned(),
            };
            let ranked_stream = eval_window_group(
                chained,
                Arc::clone(&pre_schema),
                partition_by,
                order_by,
                vec![win_spec],
                exec_ctx,
            )?;

            let mut win_fields = pre_schema.fields().to_vec();
            win_fields.push(Field::new(RANK, DataType::I64, false));
            let win_schema = Schema::try_new_arc(win_fields)?;
            let rank_filter = Expr::Binary {
                left: Box::new(col_expr_q(&win_schema, RANK)?),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::I64(1))),
                data_type: DataType::Bool,
            };

            let out_fields = pre_schema.fields()[..LATEST_SCORE_COLUMNS.len()].to_vec();
            let out_schema = Schema::try_new_arc(out_fields)?;
            let out_exprs: Vec<Expr> = LATEST_SCORE_COLUMNS
                .iter()
                .map(|name| col_expr_q(&win_schema, name))
                .collect::<Result<Vec<_>, _>>()?;

            let pool = Arc::clone(&exec_ctx.pool);
            let win_ctx = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                vec![ranked_stream],
                move |_| {
                    let f = FilterOp::new(rank_filter.clone(), win_ctx.clone());
                    let p = ProjectOp::with_schema(
                        out_exprs.clone(),
                        Arc::clone(&out_schema),
                        win_ctx.clone(),
                    )?;
                    Ok(vec![
                        Box::new(f) as Box<dyn PushOperator>,
                        Box::new(p) as Box<dyn PushOperator>,
                    ])
                },
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            Ok(sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect())
        })
        .await?;

    Ok(batches)
}

/// The live version of each score in `ids` (`scores`-view columns), in no particular order.
pub async fn scores_by_id(
    ctx: &QueryContext,
    ids: &[&str],
    timeout: Duration,
) -> Result<Vec<Batch>, RunError> {
    let ctx = ctx.engine();
    let Some(keys) = prepare_score_ids(ids)? else {
        return Ok(Vec::new());
    };
    let raw_schema = scores_raw_schema()?;
    let literals: Vec<ScalarValue> = keys.into_iter().map(ScalarValue::Utf8).collect();
    let mut filters = vec![in_list_expr(
        col_expr(&raw_schema, scores::SCORE_ID)?,
        literals,
    )];
    if let Some(vf) = ctx.visible_from {
        filters.push(cmp_ts(
            &col_expr(&raw_schema, scores::CREATED_AT)?,
            BinaryOp::GtEq,
            vf,
        ));
    }

    let spec = ScanSpec {
        projection: None,
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let scores_source = Arc::clone(&ctx.scores);
    let raw_schema_c = Arc::clone(&raw_schema);
    let (batches, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let partitions = scores_source.open_partitions(&spec, exec_ctx)?;
            if partitions.is_empty() {
                return Ok(Vec::new());
            }
            let pool = Arc::clone(&exec_ctx.pool);
            let proj_ctx = exec_ctx.clone();
            let schema_c = Arc::clone(&raw_schema_c);
            let sinks = exec_run_pipeline(
                exec_ctx,
                partitions,
                move |_| {
                    let proj = scores_view_projection(&schema_c, proj_ctx.clone())?;
                    Ok(vec![Box::new(proj) as Box<dyn PushOperator>])
                },
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            Ok(sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect())
        })
        .await?;

    Ok(batches)
}

/// Live scores per name, in each bucket or in the window of the request, sorted `(name ASC[, bucket ASC])`.
pub async fn score_name_counts(
    ctx: &QueryContext,
    request: ScoreNameCountsRequest<'_>,
) -> Result<Vec<ScoreNameCount>, RunError> {
    let ctx = ctx.engine();
    if request.names.len() > MAX_COUNT_NAMES {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_COUNT_NAMES} names"
        )));
    }
    if matches!(request.bucket_ns, Some(width) if width <= 0) {
        return Err(RunError::InvalidRequest(
            "the bucket width must be positive".to_owned(),
        ));
    }
    if request.names.is_empty() {
        return Ok(Vec::new());
    }

    let raw_schema = scores_raw_schema()?;
    let proj_indices = [scores::NAME, scores::CREATED_AT, scores::USER_ID]
        .iter()
        .map(|name| {
            raw_schema
                .index_of(name)
                .ok_or_else(|| internal(format!("scores schema missing column {name}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut min_ns = request.window.from();
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }
    let max_ns = request.window.to().checked_sub(1);

    let name_lits: Vec<ScalarValue> = request
        .names
        .iter()
        .map(|n| ScalarValue::Utf8(n.clone()))
        .collect();
    let created_raw = col_expr(&raw_schema, scores::CREATED_AT)?;
    let mut filters = vec![
        in_list_expr(col_expr(&raw_schema, scores::NAME)?, name_lits),
        cmp_ts(&created_raw, BinaryOp::GtEq, request.window.from()),
        cmp_ts(&created_raw, BinaryOp::Lt, request.window.to()),
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(cmp_ts(&created_raw, BinaryOp::GtEq, vf));
    }

    let spec = ScanSpec {
        projection: Some(proj_indices),
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let bucket_ns = request.bucket_ns;
    let window_from = request.window.from();
    let scores_source = Arc::clone(&ctx.scores);

    let (batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let scan_schema = scores_source.projected_schema(&spec)?;
            let partitions = scores_source.open_partitions(&spec, exec_ctx)?;
            if partitions.is_empty() {
                return Ok(Vec::new());
            }

            let name_col = col_expr_q(&scan_schema, scores::NAME)?;
            let created_i64 = Expr::Cast {
                expr: Box::new(col_expr_q(&scan_schema, scores::CREATED_AT)?),
                target: DataType::I64,
                mode: CastMode::Strict,
            };
            let user_col = col_expr_q(&scan_schema, scores::USER_ID)?;

            let mut group_by = vec![(scores::NAME.to_owned(), name_col)];
            if let Some(width) = bucket_ns {
                let width_lit = Expr::Literal(ScalarValue::I64(width));
                let rem = Expr::Binary {
                    left: Box::new(created_i64.clone()),
                    op: BinaryOp::Mod,
                    right: Box::new(width_lit),
                    data_type: DataType::I64,
                };
                let bucket_expr = Expr::Binary {
                    left: Box::new(created_i64.clone()),
                    op: BinaryOp::Sub,
                    right: Box::new(rem),
                    data_type: DataType::I64,
                };
                group_by.push((BUCKET.to_owned(), bucket_expr));
            }

            let agg_spec = HashAggSpec {
                group_by,
                aggs: vec![
                    AggExpr {
                        func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
                        filter: None,
                        output_name: COUNT.to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::CountDistinct(user_col),
                        filter: None,
                        output_name: USERS.to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::Max(created_i64),
                        filter: None,
                        output_name: LAST.to_owned(),
                    },
                ],
            };

            let spec_c = agg_spec.clone();
            let schema_c = Arc::clone(&scan_schema);
            let ctx_c = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                partitions,
                |_| Ok(Vec::new()),
                move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_c.clone()),
            )?;

            HashAgg::merge_sinks(sinks, &agg_spec, &scan_schema, exec_ctx)
        })
        .await?;

    let mut found = Vec::new();
    for batch in &batches {
        let name_col = require_col(batch, scores::NAME)?;
        let bucket_col = if bucket_ns.is_some() {
            Some(require_col(batch, BUCKET)?)
        } else {
            None
        };
        let count_col = require_col(batch, COUNT)?;
        let users_col = require_col(batch, USERS)?;
        let last_col = require_col(batch, LAST)?;

        for row in 0..batch.num_rows() {
            let name = string_at(batch, name_col, row)?.ok_or_else(bad_row)?;
            let bucket_start = match bucket_col {
                Some(col) => i64_at(batch, col, row)?,
                None => window_from,
            };
            let count = non_negative(i64_at(batch, count_col, row)?)?;
            let users = non_negative(i64_at(batch, users_col, row)?)?;
            let last_created_at = i64_at(batch, last_col, row)?;
            found.push(ScoreNameCount {
                name,
                bucket_start,
                count,
                users,
                last_created_at,
            });
        }
    }

    found.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.bucket_start.cmp(&b.bucket_start))
    });
    Ok(found)
}

fn project_batches_to_scores_view(
    raw_schema: &Schema,
    batches: Vec<Batch>,
    exec_ctx: &ExecContext,
) -> Result<Vec<Batch>, QueryError> {
    if batches.is_empty() {
        return Ok(Vec::new());
    }
    let mut proj = scores_view_projection(raw_schema, exec_ctx.clone())?;
    let mut dummy_res = exec_ctx.pool.register("scores_proj_in", false);
    let mut out = Vec::with_capacity(batches.len());
    for batch in batches {
        if batch.is_empty() {
            continue;
        }
        proj.push(batch, &mut dummy_res, &mut |projected, _out_res| {
            if !projected.is_empty() {
                out.push(projected);
            }
            Ok(ControlFlow::Continue)
        })?;
        dummy_res.free();
    }
    Ok(out)
}

fn prepare_score_ids(ids: &[&str]) -> Result<Option<Vec<String>>, RunError> {
    if ids.len() > MAX_PAGE {
        return Err(RunError::InvalidRequest("at most 1000 ids".to_owned()));
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let mut keys = Vec::with_capacity(ids.len());
    for id in ids {
        let uuid = uuid::Uuid::parse_str(id)
            .map_err(|_| RunError::InvalidRequest("invalid score_id".to_owned()))?;
        keys.push(uuid.to_string());
    }
    Ok(Some(keys))
}

struct PreparedKeys {
    kind: ScoreTargetKind,
    column: &'static str,
    literals: Vec<ScalarValue>,
}

fn prepare_keys(targets: ScoreTargets<'_>) -> Result<Option<PreparedKeys>, RunError> {
    let (kind, column, ids, hex_width) = match targets {
        ScoreTargets::Traces(ids) => (ScoreTargetKind::Trace, scores::TRACE_ID, ids, Some(32)),
        ScoreTargets::Sessions(ids) => (ScoreTargetKind::Session, scores::SESSION_ID, ids, None),
    };
    if ids.len() > MAX_PAGE {
        return Err(RunError::InvalidRequest("at most 1000 keys".to_owned()));
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let mut literals = Vec::with_capacity(ids.len());
    for id in ids {
        match hex_width {
            Some(width) => {
                let bytes = decode_hex_16(id, width, "trace_id")?;
                literals.push(ScalarValue::FixedBytes16(bytes));
            }
            None => literals.push(ScalarValue::Utf8((*id).to_owned())),
        }
    }
    Ok(Some(PreparedKeys {
        kind,
        column,
        literals,
    }))
}

fn list_filters(
    schema: &Schema,
    request: &ScoreListRequest<'_>,
    visible_from: Option<i64>,
) -> Result<Vec<Expr>, RunError> {
    let mut conjuncts = Vec::new();
    if let Some(target) = request.target {
        conjuncts
            .push(eq_lit_str(schema, scores::TARGET, target.as_str()).map_err(query_err_to_run)?);
    }
    if let Some(trace_id) = request.trace_id {
        let bytes = decode_hex_16(trace_id, 32, "trace_id")?;
        conjuncts.push(Expr::Binary {
            left: Box::new(col_expr(schema, scores::TRACE_ID)?),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(bytes))),
            data_type: DataType::Bool,
        });
    }
    if let Some(span_id) = request.span_id {
        let bytes = decode_hex_8(span_id, 16, "span_id")?;
        conjuncts.push(Expr::Binary {
            left: Box::new(col_expr(schema, scores::SPAN_ID)?),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::FixedBytes8(bytes))),
            data_type: DataType::Bool,
        });
    }
    if let Some(session_id) = request.session_id {
        conjuncts
            .push(eq_lit_str(schema, scores::SESSION_ID, session_id).map_err(query_err_to_run)?);
    }
    if let Some(name) = request.name {
        conjuncts.push(eq_lit_str(schema, scores::NAME, name).map_err(query_err_to_run)?);
    }
    if let Some(source) = request.source {
        conjuncts
            .push(eq_lit_str(schema, scores::SOURCE, source.as_str()).map_err(query_err_to_run)?);
    }
    if let Some(evaluator_id) = request.evaluator_id {
        conjuncts.push(
            eq_lit_str(schema, scores::EVALUATOR_ID, evaluator_id).map_err(query_err_to_run)?,
        );
    }
    if let Some(rule_id) = request.rule_id {
        conjuncts.push(eq_lit_str(schema, scores::RULE_ID, rule_id).map_err(query_err_to_run)?);
    }
    if let Some(queue_id) = request.queue_id {
        conjuncts.push(eq_lit_str(schema, scores::QUEUE_ID, queue_id).map_err(query_err_to_run)?);
    }
    if let Some(window) = request.window {
        let created = col_expr(schema, scores::CREATED_AT)?;
        conjuncts.push(cmp_ts(&created, BinaryOp::GtEq, window.from()));
        conjuncts.push(cmp_ts(&created, BinaryOp::Lt, window.to()));
    }
    if let Some(vf) = visible_from {
        let created = col_expr(schema, scores::CREATED_AT)?;
        conjuncts.push(cmp_ts(&created, BinaryOp::GtEq, vf));
    }
    Ok(conjuncts)
}

fn decode_hex_16(text: &str, width: usize, name: &str) -> Result<[u8; 16], RunError> {
    if text.len() == width && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        let bytes = hex::decode(text)
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")))?;
        return bytes
            .try_into()
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")));
    }
    Err(RunError::InvalidRequest(format!(
        "{name} must be {width} hex digits"
    )))
}

fn decode_hex_8(text: &str, width: usize, name: &str) -> Result<[u8; 8], RunError> {
    if text.len() == width && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        let bytes = hex::decode(text)
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")))?;
        return bytes
            .try_into()
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")));
    }
    Err(RunError::InvalidRequest(format!(
        "{name} must be {width} hex digits"
    )))
}

fn col_expr(schema: &Schema, name: &str) -> Result<Expr, RunError> {
    col_expr_q(schema, name).map_err(query_err_to_run)
}

fn col_expr_q(schema: &Schema, name: &str) -> Result<Expr, QueryError> {
    let idx = schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("missing column {name} in schema")))?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

fn eq_lit_str(schema: &Schema, col_name: &str, value: &str) -> Result<Expr, QueryError> {
    Ok(Expr::Binary {
        left: Box::new(col_expr_q(schema, col_name)?),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(ScalarValue::Utf8(value.to_owned()))),
        data_type: DataType::Bool,
    })
}

fn in_list_expr(col: Expr, literals: Vec<ScalarValue>) -> Expr {
    Expr::InList {
        expr: Box::new(col),
        list: literals.into_iter().map(Expr::Literal).collect(),
        negated: false,
        redacted: false,
    }
}

fn cmp_ts(col: &Expr, op: BinaryOp, ns: i64) -> Expr {
    Expr::Binary {
        left: Box::new(col.clone()),
        op,
        right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
            value: ns,
            utc: true,
        })),
        data_type: DataType::Bool,
    }
}

fn non_negative(value: i64) -> Result<u64, RunError> {
    u64::try_from(value).map_err(|_| bad_row())
}

fn bad_row() -> RunError {
    internal("a view returned a null where a value is required")
}

struct ChainedStream {
    streams: std::collections::VecDeque<Box<dyn PartitionStream>>,
}

impl PartitionStream for ChainedStream {
    fn next_batch(
        &mut self,
        cancel: &cetra_column::CancelToken,
        mut res: Option<&mut cetra_column::Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        while let Some(front) = self.streams.front_mut() {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            match front.next_batch(cancel, res.as_deref_mut())? {
                Some(batch) => return Ok(Some(batch)),
                None => {
                    self.streams.pop_front();
                }
            }
        }
        Ok(None)
    }
}

fn chain_streams(streams: Vec<Box<dyn PartitionStream>>) -> Box<dyn PartitionStream> {
    Box::new(ChainedStream {
        streams: streams.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::ScoreSource;
    use cetra_core::ids::ProjectId;
    use cetra_core::score::{Score, ScoreValue};
    use cetra_core::time_serde::parse_rfc3339;
    use cetra_query::encode::{SQL_MAX_RESULT_BYTES, SQL_MAX_ROWS, encode_json_result};
    use serde_json::{Map, Value};

    use super::*;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::deletion::{Deletion, DeletionScope};
    use crate::reads::MAX_PAGE;
    use crate::test_support::{dataset_scores, dataset_spans, score_row, score_rows, span_rows};
    use crate::views::{QueryContext, TimeWindow};
    use crate::{QueryOptions, Storage};

    const TIMEOUT: Duration = Duration::from_secs(30);
    const DAY: i64 = 86_400_000_000_000;

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

    fn qe_rows(batches: &[Batch]) -> Vec<Map<String, Value>> {
        let non_empty: Vec<Batch> = batches
            .iter()
            .filter(|b| b.num_rows() > 0)
            .cloned()
            .collect();
        let Some(first) = non_empty.first() else {
            return Vec::new();
        };
        encode_json_result(
            first.schema(),
            &non_empty,
            SQL_MAX_ROWS,
            SQL_MAX_RESULT_BYTES,
            false,
            None,
        )
        .expect("encode json")
        .rows
    }

    async fn build_context(
        storage: &Storage,
        project: ProjectId,
        options: QueryOptions,
    ) -> QueryContext {
        QueryContext::new(storage.query(project, options).await.expect("query"), None).expect("ctx")
    }

    fn base_req() -> ScoreListRequest<'static> {
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
            window: None,
            limit: 50,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    #[tokio::test]
    async fn score_reads_pagination_latest_by_id_and_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        // Flush dataset spans & dataset scores into Parquet (L0).
        let mut ds_spans = dataset_spans();
        for s in &mut ds_spans {
            s.project_id = project;
        }
        storage
            .append_spans(project, span_rows(&ds_spans))
            .await
            .expect("spans");
        let mut ds_scores = dataset_scores();
        // Add an issue score + tied updated_at scores + deleted update + multi-day scores.
        let t0 = ds_scores[0].target.clone();
        let mut issue_score = score_row(100, 0, 10 * DAY + 100, 100, 100, false, "issue:err");
        issue_score.target = t0.clone();
        issue_score.source = ScoreSource::Issue;
        issue_score.user_id = Some("u1".to_owned());

        let mut tie_low = score_row(101, 0, 10 * DAY + 200, 200, 200, false, "tie_score");
        tie_low.score_id = "01900000-0000-7000-8000-0000000009a1"
            .parse()
            .expect("uuid");
        tie_low.target = t0.clone();
        tie_low.value = ScoreValue::Numeric(0.2);
        tie_low.user_id = Some("u2".to_owned());

        let mut tie_high = score_row(102, 0, 10 * DAY + 200, 200, 200, false, "tie_score");
        tie_high.score_id = "01900000-0000-7000-8000-0000000009a2"
            .parse()
            .expect("uuid");
        tie_high.target = t0.clone();
        tie_high.value = ScoreValue::Numeric(0.8);
        tie_high.user_id = Some("u1".to_owned());

        let mut to_delete_old = score_row(103, 0, 10 * DAY + 300, 100, 100, false, "issue:err");
        to_delete_old.target = t0.clone();
        to_delete_old.user_id = Some("u3".to_owned());

        let mut erased_user_score =
            score_row(104, 0, 10 * DAY + 400, 100, 100, false, "erased_score");
        erased_user_score.target = t0;
        erased_user_score.user_id = Some("erased-user".to_owned());

        ds_scores.extend([
            issue_score,
            tie_low,
            tie_high,
            to_delete_old.clone(),
            erased_user_score,
        ]);
        for s in &mut ds_scores {
            s.project_id = project;
        }
        storage
            .append_scores(project, score_rows(&ds_scores))
            .await
            .expect("append scores 1");
        storage
            .flush_now(Duration::from_secs(10))
            .await
            .expect("flush");

        // Append memtable scores: a tombstone-deleting LWW update + day2 score + evaluator/rule/queue scores.
        let mut to_delete_new = to_delete_old;
        to_delete_new.updated_at = 500;
        to_delete_new.ingest_time = 500;
        to_delete_new.deleted = true;

        let mut day2_score: Score = score_row(105, 1, 11 * DAY + 50, 100, 100, false, "issue:err");
        day2_score.evaluator_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e1".parse().expect("ev"));
        day2_score.rule_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000f1".parse().expect("rl"));
        day2_score.queue_id = Some("018f6b4e-7c1a-7c1a-8c1a-0000000000a1".parse().expect("qu"));

        let mut mem_scores = vec![to_delete_new, day2_score];
        for s in &mut mem_scores {
            s.project_id = project;
        }
        storage
            .append_scores(project, score_rows(&mem_scores))
            .await
            .expect("append scores 2");

        storage
            .register_deletion(Deletion {
                id: "018f6b4e-7c1a-7c1a-8c1a-0000000000d1".parse().expect("did"),
                project,
                cutoff: i64::MAX,
                scope: DeletionScope::User {
                    user_id: "erased-user".to_owned(),
                    trace_ids: Vec::new(),
                    session_ids: Vec::new(),
                },
            })
            .await
            .expect("register deletion");

        for vf in [
            None,
            Some(parse_rfc3339("2026-01-01T10:07:00Z").expect("vf")),
        ] {
            let ctx = build_context(&storage, project, QueryOptions::new(None, vf)).await;

            // 1. score_list full + cursor walk + filters
            let full = score_list(&ctx, base_req()).await.expect("score_list");
            assert!(full.next_cursor.is_none());
            let full_rows = qe_rows(&full.batches);
            assert!(!full_rows.is_empty());
            // Deleted & erased-user scores must never appear.
            assert!(
                !full_rows.iter().any(|r| r["name"] == "erased_score"),
                "erased_user_score leaked"
            );

            // Walk with limit = 2 cursor-for-cursor and verify it reassembles `full_rows`.
            let mut cursor: Option<String> = None;
            let mut walked = Vec::new();
            loop {
                let mut req = base_req();
                req.limit = 2;
                req.cursor = cursor.as_deref();
                let page = score_list(&ctx, req).await.expect("page");
                let rows = qe_rows(&page.batches);
                assert!(rows.len() <= 2);
                walked.extend(rows);
                match page.next_cursor {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            assert_eq!(walked, full_rows, "cursor walk mismatch vf={vf:?}");

            // Equality conjuncts & window
            let by_issue = score_list(
                &ctx,
                ScoreListRequest {
                    source: Some(ScoreSource::Issue),
                    ..base_req()
                },
            )
            .await
            .expect("source issue");
            assert!(
                qe_rows(&by_issue.batches)
                    .iter()
                    .all(|r| r["source"] == "issue")
            );
            let by_ids = score_list(
                &ctx,
                ScoreListRequest {
                    evaluator_id: Some("018f6b4e-7c1a-7c1a-8c1a-0000000000e1"),
                    rule_id: Some("018f6b4e-7c1a-7c1a-8c1a-0000000000f1"),
                    queue_id: Some("018f6b4e-7c1a-7c1a-8c1a-0000000000a1"),
                    ..base_req()
                },
            )
            .await
            .expect("ev/rl/qu");
            let id_rows = qe_rows(&by_ids.batches);
            assert_eq!(id_rows.len(), if vf.is_some() { 0 } else { 1 });

            // 2. latest_scores (traces + sessions, excludes source = 'issue', tie-breaks score_id DESC)
            let trace_keys = [
                "A1000000000000000000000000000001",
                "a1000000000000000000000000000002",
            ];
            let q_lat_t = latest_scores(&ctx, ScoreTargets::Traces(&trace_keys), TIMEOUT)
                .await
                .expect("latest traces");
            let q_lat_json = qe_rows(&q_lat_t);
            assert!(!q_lat_json.iter().any(|r| r["source"] == "issue"));
            if let Some(tie_row) = q_lat_json.iter().find(|r| r["name"] == "tie_score") {
                assert_eq!(tie_row["score_id"], "01900000-0000-7000-8000-0000000009a2");
            }

            let sess_keys = ["sess-001", "sess-002"];
            let q_lat_s = latest_scores(&ctx, ScoreTargets::Sessions(&sess_keys), TIMEOUT)
                .await
                .expect("latest sessions");
            let _ = qe_rows(&q_lat_s);

            // 3. scores_by_id
            let ids = [
                "01900000-0000-7000-8000-000000000301",
                "01900000-0000-7000-8000-000000000302",
                "01900000-0000-7000-8000-0000000009A2",
            ];
            let mut by_id = qe_rows(&scores_by_id(&ctx, &ids, TIMEOUT).await.expect("by_id"));
            by_id.sort_by(|a, b| a["score_id"].as_str().cmp(&b["score_id"].as_str()));
            if vf.is_none() {
                assert!(
                    by_id
                        .iter()
                        .any(|r| r["score_id"] == "01900000-0000-7000-8000-0000000009a2")
                );
            }

            // 4. score_name_counts
            let names = vec!["issue:err".to_owned(), "tie_score".to_owned()];
            let window = TimeWindow::new(0, 100 * DAY).expect("window");
            for bucket_ns in [Some(DAY), None] {
                let req = ScoreNameCountsRequest {
                    names: &names,
                    window,
                    bucket_ns,
                    timeout: TIMEOUT,
                };
                let counts = score_name_counts(&ctx, req).await.expect("counts");
                if vf.is_none() {
                    assert!(
                        !counts.is_empty(),
                        "expected non-empty counts bucket_ns={bucket_ns:?}"
                    );
                }
            }
        }

        // Error checks
        let ctx = build_context(&storage, project, QueryOptions::new(None, None)).await;
        assert!(matches!(
            score_list(
                &ctx,
                ScoreListRequest {
                    limit: 0,
                    ..base_req()
                }
            )
            .await,
            Err(RunError::InvalidRequest(_))
        ));
        assert!(matches!(
            score_list(
                &ctx,
                ScoreListRequest {
                    trace_id: Some("bad"),
                    ..base_req()
                }
            )
            .await,
            Err(RunError::InvalidRequest(_))
        ));
        assert!(matches!(
            scores_by_id(&ctx, &["not-a-uuid"], TIMEOUT).await,
            Err(RunError::InvalidRequest(m)) if m == "invalid score_id"
        ));
        assert!(
            latest_scores(&ctx, ScoreTargets::Traces(&[]), TIMEOUT)
                .await
                .expect("empty")
                .is_empty()
        );
        let too_many = vec!["a1000000000000000000000000000001"; MAX_PAGE + 1];
        assert!(matches!(
            latest_scores(&ctx, ScoreTargets::Traces(&too_many), TIMEOUT).await,
            Err(RunError::InvalidRequest(_))
        ));
        let too_many_names = vec!["x".to_owned(); MAX_COUNT_NAMES + 1];
        assert!(matches!(
            score_name_counts(
                &ctx,
                ScoreNameCountsRequest {
                    names: &too_many_names,
                    window: TimeWindow::new(0, DAY).expect("w"),
                    bucket_ns: None,
                    timeout: TIMEOUT,
                }
            )
            .await,
            Err(RunError::InvalidRequest(_))
        ));
        for width in [0, -1] {
            assert!(matches!(
                score_name_counts(
                    &ctx,
                    ScoreNameCountsRequest {
                        names: &[],
                        window: TimeWindow::new(0, DAY).expect("w"),
                        bucket_ns: Some(width),
                        timeout: TIMEOUT,
                    }
                )
                .await,
                Err(RunError::InvalidRequest(_))
            ));
        }

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn score_name_counts_by_hour_and_non_positive_width() {
        const HOUR: i64 = 3_600_000_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000002".parse().expect("pid");

        let day1 = 10 * DAY;
        let day2 = 11 * DAY;
        let mut s1 = score_row(1, 1, day1 + 100, day1 + 100, 1, false, "issue:a");
        s1.user_id = Some("u1".to_owned());
        let mut s2 = score_row(2, 1, day1 + HOUR + 5, day1 + HOUR + 5, 2, false, "issue:a");
        s2.user_id = Some("u2".to_owned());
        let mut s3 = score_row(3, 1, day2 + 50, day2 + 50, 3, false, "issue:a");
        s3.user_id = None;
        let mut s4 = score_row(4, 1, day1 + 400, day1 + 400, 4, false, "issue:b");
        s4.user_id = Some("u1".to_owned());
        let mut rows = vec![s1, s2, s3, s4];
        for r in &mut rows {
            r.project_id = project;
        }
        storage
            .append_scores(project, score_rows(&rows))
            .await
            .expect("append scores");

        let ctx = build_context(&storage, project, QueryOptions::new(None, None)).await;
        let names = ["issue:a".to_owned(), "issue:b".to_owned()];
        let window = TimeWindow::new(0, 100 * DAY).expect("window");

        let counts = score_name_counts(
            &ctx,
            ScoreNameCountsRequest {
                names: &names,
                window,
                bucket_ns: Some(HOUR),
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("by hour");
        assert_eq!(
            counts,
            [
                ScoreNameCount {
                    name: "issue:a".to_owned(),
                    bucket_start: day1,
                    count: 1,
                    users: 1,
                    last_created_at: day1 + 100,
                },
                ScoreNameCount {
                    name: "issue:a".to_owned(),
                    bucket_start: day1 + HOUR,
                    count: 1,
                    users: 1,
                    last_created_at: day1 + HOUR + 5,
                },
                ScoreNameCount {
                    name: "issue:a".to_owned(),
                    bucket_start: day2,
                    count: 1,
                    users: 0,
                    last_created_at: day2 + 50,
                },
                ScoreNameCount {
                    name: "issue:b".to_owned(),
                    bucket_start: day1,
                    count: 1,
                    users: 1,
                    last_created_at: day1 + 400,
                },
            ]
        );

        let none: Vec<String> = Vec::new();
        for (list, width) in [(&names[..], 0), (&names[..], -1), (&none[..], 0)] {
            assert!(matches!(
                score_name_counts(
                    &ctx,
                    ScoreNameCountsRequest {
                        names: list,
                        window,
                        bucket_ns: Some(width),
                        timeout: TIMEOUT,
                    }
                )
                .await,
                Err(RunError::InvalidRequest(_))
            ));
        }

        let _ = storage.shutdown().await;
    }
}
