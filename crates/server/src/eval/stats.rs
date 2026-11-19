//! Evaluator stats of `GET /evaluators/stats` (D-I-48): the job figures come from `meta::eval_jobs`,
//! the score and experiment figures from the `scores` view through fixed statements. The pure
//! assembly sits here too, so its rules are tested without a server.

use std::collections::HashMap;

use cetra_core::enums::ScoreDataType;
use cetra_core::time_serde::parse_rfc3339;
use cetra_storage::{QueryContext, SQL_MAX_ROWS, SqlRequest};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::api::evaluators as api;
use crate::api::metrics::MetricPoint;
use crate::http::error::ApiError;
use crate::meta::MetaError;
use crate::meta::eval_jobs::{self, DAY_NS, HOUR_NS, RULE_STATS_HOURS, STATS_BACK_NS, STATS_DAYS};
use crate::routes::read::Deadline;

/// Days back in which a score with an experiment counts for `experiments_with_score` (D-I-48).
pub(crate) const EXPERIMENT_DAYS: i64 = 30;
/// Experiments listed in `recent`.
pub(crate) const RECENT_EXPERIMENTS: usize = 5;
/// Evaluator ids one request may name.
pub(crate) const MAX_RESTRICT: usize = 50;

/// The evaluator stats window at `now`: from the start of the UTC day six days ago, to `now`
/// (exclusive).
pub(crate) fn window(now: i64) -> (i64, i64) {
    (now - now.rem_euclid(DAY_NS) - STATS_BACK_NS, now)
}

/// The online rule stats window at `now` (D-I-50): the last [`RULE_STATS_HOURS`] hours, the
/// current one included, up to `now` (exclusive).
pub(crate) fn rule_window(now: i64) -> (i64, i64) {
    let back = (RULE_STATS_HOURS as i64 - 1) * HOUR_NS;
    (now - now.rem_euclid(HOUR_NS) - back, now)
}

/// Start of bucket `index` of a window that starts at `from` and has buckets of `bucket_ns`.
pub(crate) fn bucket_time(from: i64, bucket_ns: i64, index: usize) -> i64 {
    from + index as i64 * bucket_ns
}

/// What a score read groups by: the evaluator (D-I-48, daily buckets) or the online rule (D-I-50,
/// hourly buckets). Both read the `scores` view through the same statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScoreGroup {
    Evaluator,
    Rule,
}

impl ScoreGroup {
    /// The `scores` column that names the group.
    fn column(self) -> &'static str {
        match self {
            Self::Evaluator => "evaluator_id",
            Self::Rule => "rule_id",
        }
    }

    /// The `date_trunc` unit of the buckets.
    fn unit(self) -> &'static str {
        match self {
            Self::Evaluator => "day",
            Self::Rule => "hour",
        }
    }

    /// Nanoseconds per bucket.
    pub(crate) fn bucket_ns(self) -> i64 {
        match self {
            Self::Evaluator => DAY_NS,
            Self::Rule => HOUR_NS,
        }
    }

    /// Buckets in the window.
    pub(crate) fn buckets(self) -> usize {
        match self {
            Self::Evaluator => STATS_DAYS,
            Self::Rule => RULE_STATS_HOURS,
        }
    }
}

/// A row the statements return that does not have the shape the statement promises.
fn malformed(column: &'static str) -> ApiError {
    ApiError::Internal(Box::new(MetaError::Corrupt {
        table: "scores",
        column,
    }))
}

/// A statement that reached its row limit: the answer would be partial, so the request fails.
#[derive(Debug, thiserror::Error)]
#[error("evaluator stats statement returned more than {0} rows")]
struct TooManyRows(usize);

fn ensure_complete(truncated: bool) -> Result<(), ApiError> {
    if truncated {
        Err(ApiError::Internal(Box::new(TooManyRows(SQL_MAX_ROWS))))
    } else {
        Ok(())
    }
}

/// The live scores of one group (evaluator or rule) in the window, with the numeric and
/// per-bucket figures.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ScoreAggregate {
    /// Live scores of any type.
    pub(crate) count: u64,
    /// Scores with a numeric value.
    pub(crate) numeric_count: u64,
    /// Sum of the numeric values.
    pub(crate) numeric_sum: f64,
    /// `(numeric count, numeric sum)` per window bucket, oldest first.
    pub(crate) buckets: Vec<(u64, f64)>,
    /// The most frequent categorical label and its count (ties to the smaller label).
    pub(crate) top_category: Option<(String, u64)>,
}

/// `AND {column} IN (..)` with its parameters, or nothing when `ids` is `None`.
fn restrict_clause(column: &str, ids: Option<&[Uuid]>) -> (String, Map<String, Value>) {
    let mut params = Map::new();
    let Some(ids) = ids else {
        return (String::new(), params);
    };
    let mut names = Vec::with_capacity(ids.len());
    for (index, id) in ids.iter().enumerate() {
        let name = format!("e{index}");
        names.push(format!("{{{name}:String}}"));
        params.insert(name, Value::String(id.to_string()));
    }
    (format!(" AND {column} IN ({})", names.join(", ")), params)
}

fn field_u64(row: &Map<String, Value>, column: &'static str) -> Result<u64, ApiError> {
    row.get(column)
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed(column))
}

fn field_str<'a>(row: &'a Map<String, Value>, column: &'static str) -> Result<&'a str, ApiError> {
    row.get(column)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(column))
}

fn field_time(row: &Map<String, Value>, column: &'static str) -> Result<i64, ApiError> {
    parse_rfc3339(field_str(row, column)?).map_err(|_| malformed(column))
}

/// Reads the scores of `ctx` (window `[from, ..)`) per group `group`, in buckets of
/// `group.bucket_ns()`. `restrict` names the groups when the request did; `with_categories` runs
/// the categorical statement, which only the categorical groups need.
///
/// # Errors
///
/// Returns [`ApiError`] when a statement fails, runs out of `deadline`, reaches its row limit or
/// returns a row of the wrong shape.
pub(crate) async fn read_scores(
    ctx: &QueryContext,
    group: ScoreGroup,
    from: i64,
    restrict: Option<&[Uuid]>,
    with_categories: bool,
    deadline: Deadline,
) -> Result<HashMap<Uuid, ScoreAggregate>, ApiError> {
    if restrict.is_some_and(<[Uuid]>::is_empty) {
        return Ok(HashMap::new());
    }
    let column = group.column();
    let unit = group.unit();
    let bucket_ns = group.bucket_ns();
    let buckets = group.buckets();
    let (clause, params) = restrict_clause(column, restrict);
    let bucket_query = format!(
        "SELECT {column}, date_trunc('{unit}', created_at) AS bucket, count(*) AS n, \
         count(value_num) AS num_n, sum(value_num) AS num_sum FROM scores \
         WHERE {column} IS NOT NULL AND deleted = false{clause} \
         GROUP BY {column}, date_trunc('{unit}', created_at)"
    );
    let json = ctx
        .sql_json(SqlRequest {
            query: &bucket_query,
            params: &params,
            limit: SQL_MAX_ROWS,
            timeout: deadline.remaining()?,
        })
        .await
        .map_err(ApiError::Query)?;
    ensure_complete(json.truncated)?;

    let fresh = || ScoreAggregate {
        buckets: vec![(0, 0.0); buckets],
        ..Default::default()
    };
    let mut aggregates: HashMap<Uuid, ScoreAggregate> = HashMap::new();
    for row in &json.rows {
        let key = field_str(row, column)?
            .parse::<Uuid>()
            .map_err(|_| malformed(column))?;
        let bucket = field_time(row, "bucket")?;
        if bucket < from {
            continue;
        }
        let Some(index) = usize::try_from((bucket - from) / bucket_ns)
            .ok()
            .filter(|index| *index < buckets)
        else {
            continue;
        };
        let count = field_u64(row, "n")?;
        let entry = aggregates.entry(key).or_insert_with(fresh);
        entry.count += count;
        // A null sum means the bucket has no numeric value (NaN encodes as null, sql/encode.rs).
        let sum = row.get("num_sum").ok_or_else(|| malformed("num_sum"))?;
        if sum.is_null() {
            continue;
        }
        let numeric_count = field_u64(row, "num_n")?;
        let sum = sum.as_f64().ok_or_else(|| malformed("num_sum"))?;
        entry.numeric_count += numeric_count;
        entry.numeric_sum += sum;
        entry.buckets[index].0 += numeric_count;
        entry.buckets[index].1 += sum;
    }

    if with_categories {
        let query = format!(
            "SELECT {column}, value_str, n FROM (SELECT {column}, value_str, n, \
             row_number() OVER (PARTITION BY {column} ORDER BY n DESC, value_str ASC) AS rn \
             FROM (SELECT {column}, value_str, count(*) AS n FROM scores \
             WHERE {column} IS NOT NULL AND deleted = false AND data_type = 'categorical' \
             AND value_str IS NOT NULL{clause} GROUP BY {column}, value_str)) WHERE rn = 1"
        );
        let json = ctx
            .sql_json(SqlRequest {
                query: &query,
                params: &params,
                limit: SQL_MAX_ROWS,
                timeout: deadline.remaining()?,
            })
            .await
            .map_err(ApiError::Query)?;
        ensure_complete(json.truncated)?;
        for row in &json.rows {
            let key = field_str(row, column)?
                .parse::<Uuid>()
                .map_err(|_| malformed(column))?;
            let label = field_str(row, "value_str")?.to_owned();
            let count = field_u64(row, "n")?;
            aggregates.entry(key).or_insert_with(fresh).top_category = Some((label, count));
        }
    }
    Ok(aggregates)
}

/// An experiment with a score of a given name, from `read_experiment_scores`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScoredExperiment {
    /// The score name.
    pub(crate) name: String,
    /// The experiment.
    pub(crate) experiment_id: Uuid,
    /// The latest score time of this experiment and name, nanoseconds.
    pub(crate) last_scored_at: i64,
}

/// Experiments with a live score per `(name, experiment)` in `ctx`'s window, for the score names
/// `names` when given. The window is the last [`EXPERIMENT_DAYS`] days.
///
/// # Errors
///
/// Returns [`ApiError`] when a statement fails, runs out of `deadline`, reaches its row limit or
/// returns a row of the wrong shape.
pub(crate) async fn read_experiment_scores(
    ctx: &QueryContext,
    names: Option<&[String]>,
    deadline: Deadline,
) -> Result<Vec<ScoredExperiment>, ApiError> {
    if names.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let mut params = Map::new();
    let clause = match names {
        None => String::new(),
        Some(names) => {
            let mut placeholders = Vec::with_capacity(names.len());
            for (index, name) in names.iter().enumerate() {
                let key = format!("n{index}");
                placeholders.push(format!("{{{key}:String}}"));
                params.insert(key, Value::String(name.clone()));
            }
            format!(" AND name IN ({})", placeholders.join(", "))
        }
    };
    let query = format!(
        "SELECT name, experiment_id, max(created_at) AS last_at FROM scores \
         WHERE experiment_id IS NOT NULL AND deleted = false{clause} \
         GROUP BY name, experiment_id"
    );
    let json = ctx
        .sql_json(SqlRequest {
            query: &query,
            params: &params,
            limit: SQL_MAX_ROWS,
            timeout: deadline.remaining()?,
        })
        .await
        .map_err(ApiError::Query)?;
    ensure_complete(json.truncated)?;
    let mut scored = Vec::with_capacity(json.rows.len());
    for row in &json.rows {
        scored.push(ScoredExperiment {
            name: field_str(row, "name")?.to_owned(),
            experiment_id: field_str(row, "experiment_id")?
                .parse::<Uuid>()
                .map_err(|_| malformed("experiment_id"))?,
            last_scored_at: field_time(row, "last_at")?,
        });
    }
    Ok(scored)
}

/// The job figures of one evaluator. `jobs` and `latency` are `None` when it has no finished job.
pub(crate) fn job_stats(
    jobs: Option<&eval_jobs::EvaluatorJobStats>,
    latency: Option<(u64, u64)>,
    from: i64,
) -> api::EvaluatorJobStats {
    let zero = eval_jobs::EvaluatorJobStats::default();
    let jobs = jobs.unwrap_or(&zero);
    let (latency_p50_ms, latency_p95_ms) = match latency {
        Some((p50, p95)) => (Some(p50), Some(p95)),
        None => (None, None),
    };
    api::EvaluatorJobStats {
        done: jobs.done,
        failed: jobs.failed,
        skipped: jobs.skipped,
        cost_usd_total: jobs.cost_total,
        cost_usd_avg: (jobs.cost_jobs > 0).then(|| jobs.cost_total / jobs.cost_jobs as f64),
        latency_p50_ms,
        latency_p95_ms,
        runs_daily: jobs
            .daily
            .iter()
            .enumerate()
            .map(|(index, count)| MetricPoint {
                time: bucket_time(from, DAY_NS, index),
                value: Some(*count as f64),
            })
            .collect(),
    }
}

/// The score figures of one group, read with its current output type (D-I-48, D-I-50): the
/// summary values, and a series of `buckets` points of `bucket_ns`, oldest first. A bucket
/// without a numeric value has no `value`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScoreSummary {
    /// Mean of the numeric values. Numeric outputs only.
    pub(crate) average: Option<f64>,
    /// Share of `true` values, 0 to 1. Boolean outputs only.
    pub(crate) pass_rate: Option<f64>,
    /// The most frequent label. Categorical outputs only.
    pub(crate) top_category: Option<api::CategoryCount>,
    /// The average (numeric) or pass rate (boolean) per bucket.
    pub(crate) series: Vec<MetricPoint>,
}

pub(crate) fn score_summary(
    data_type: ScoreDataType,
    aggregate: Option<&ScoreAggregate>,
    from: i64,
    bucket_ns: i64,
    buckets: usize,
) -> ScoreSummary {
    let empty = ScoreAggregate::default();
    let aggregate = aggregate.unwrap_or(&empty);
    let has_numeric = aggregate.numeric_count > 0;
    let ratio = (has_numeric).then(|| aggregate.numeric_sum / aggregate.numeric_count as f64);
    let (average, pass_rate) = match data_type {
        ScoreDataType::Numeric => (ratio, None),
        ScoreDataType::Boolean => (None, ratio),
        _ => (None, None),
    };
    let series = (0..buckets)
        .map(|index| {
            let time = bucket_time(from, bucket_ns, index);
            let value = match data_type {
                ScoreDataType::Numeric | ScoreDataType::Boolean => {
                    let (numeric_count, sum) =
                        aggregate.buckets.get(index).copied().unwrap_or((0, 0.0));
                    (numeric_count > 0).then(|| sum / numeric_count as f64)
                }
                _ => None,
            };
            MetricPoint { time, value }
        })
        .collect();
    let top_category = match data_type {
        ScoreDataType::Categorical => {
            aggregate
                .top_category
                .as_ref()
                .map(|(label, count)| api::CategoryCount {
                    label: label.clone(),
                    count: *count,
                })
        }
        _ => None,
    };
    ScoreSummary {
        average,
        pass_rate,
        top_category,
        series,
    }
}

/// The experiments of one score name, newest score first, then by id.
pub(crate) fn experiments_for<'a>(
    scored: &'a [ScoredExperiment],
    name: &str,
) -> Vec<&'a ScoredExperiment> {
    let mut rows: Vec<&ScoredExperiment> = scored.iter().filter(|row| row.name == name).collect();
    rows.sort_by(|a, b| {
        b.last_scored_at
            .cmp(&a.last_scored_at)
            .then(a.experiment_id.cmp(&b.experiment_id))
    });
    rows
}

#[cfg(test)]
mod tests {
    use cetra_storage::TimeWindow;
    use serde_json::json;

    use super::*;
    use crate::routes::read::{Deadline, open_query_windowed};
    use crate::test_support::{TestApp, TestScore, put_score};
    use cetra_core::score::ScoreValue;

    const DAY: i64 = 86_400_000_000_000;

    fn ns(text: &str) -> i64 {
        parse_rfc3339(text).unwrap()
    }

    #[test]
    fn window_is_seven_utc_days_ending_now() {
        let now = ns("2026-01-07T13:00:00Z");
        assert_eq!(
            window(now),
            (ns("2026-01-01T00:00:00Z"), now),
            "six days back from the start of today"
        );
        let midnight = ns("2026-01-07T00:00:00Z");
        assert_eq!(window(midnight).0, ns("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn job_stats_without_jobs_has_zeros_and_no_averages() {
        let from = ns("2026-01-01T00:00:00Z");
        let stats = job_stats(None, None, from);
        assert_eq!(stats.done, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.cost_usd_total, 0.0);
        assert_eq!(stats.cost_usd_avg, None);
        assert_eq!(stats.latency_p50_ms, None);
        assert_eq!(stats.latency_p95_ms, None);
        assert_eq!(stats.runs_daily.len(), STATS_DAYS);
        assert!(
            stats
                .runs_daily
                .iter()
                .all(|point| point.value == Some(0.0))
        );
        assert_eq!(stats.runs_daily[1].time, from + DAY);
    }

    #[test]
    fn job_stats_cost_average_counts_only_jobs_with_a_cost() {
        let jobs = eval_jobs::EvaluatorJobStats {
            done: 2,
            cost_total: 0.06,
            cost_jobs: 3,
            ..Default::default()
        };
        let stats = job_stats(Some(&jobs), Some((5, 9)), 0);
        assert_eq!(stats.latency_p50_ms, Some(5));
        assert_eq!(stats.latency_p95_ms, Some(9));
        let avg = stats.cost_usd_avg.unwrap();
        assert!((avg - 0.02).abs() < 1e-12);
    }

    #[test]
    fn score_summary_numeric_boolean_categorical_text() {
        let from = ns("2026-01-01T00:00:00Z");
        let mut numeric = ScoreAggregate {
            count: 3,
            numeric_count: 3,
            numeric_sum: 2.4,
            buckets: vec![(0, 0.0); STATS_DAYS],
            ..Default::default()
        };
        numeric.buckets[0] = (2, 1.6);
        numeric.buckets[2] = (1, 0.8);
        let summary = score_summary(
            ScoreDataType::Numeric,
            Some(&numeric),
            from,
            DAY_NS,
            STATS_DAYS,
        );
        assert!((summary.average.unwrap() - 0.8).abs() < 1e-12);
        assert_eq!(summary.pass_rate, None);
        assert_eq!(summary.series.len(), STATS_DAYS);
        assert_eq!(summary.series[0].value, Some(0.8));
        assert_eq!(
            summary.series[1].value, None,
            "a bucket without numeric value"
        );
        assert!((summary.series[2].value.unwrap() - 0.8).abs() < 1e-12);

        let boolean = ScoreAggregate {
            count: 4,
            numeric_count: 4,
            numeric_sum: 3.0,
            buckets: vec![
                (4, 3.0),
                (0, 0.0),
                (0, 0.0),
                (0, 0.0),
                (0, 0.0),
                (0, 0.0),
                (0, 0.0),
            ],
            ..Default::default()
        };
        let summary = score_summary(
            ScoreDataType::Boolean,
            Some(&boolean),
            from,
            DAY_NS,
            STATS_DAYS,
        );
        assert!((summary.pass_rate.unwrap() - 0.75).abs() < 1e-12);
        assert_eq!(summary.average, None);
        assert_eq!(summary.series[0].value, Some(0.75));

        let categorical = ScoreAggregate {
            count: 3,
            top_category: Some(("pass".to_owned(), 2)),
            ..Default::default()
        };
        let summary = score_summary(
            ScoreDataType::Categorical,
            Some(&categorical),
            from,
            DAY_NS,
            STATS_DAYS,
        );
        assert_eq!(
            summary.top_category,
            Some(api::CategoryCount {
                label: "pass".to_owned(),
                count: 2
            })
        );
        assert!(summary.series.iter().all(|point| point.value.is_none()));

        let summary = score_summary(
            ScoreDataType::Text,
            Some(&categorical),
            from,
            DAY_NS,
            STATS_DAYS,
        );
        assert_eq!(summary.top_category, None);
        assert_eq!(summary.average, None);
        assert_eq!(summary.pass_rate, None);
    }

    #[test]
    fn score_summary_with_no_numeric_rows_has_no_average() {
        let from = ns("2026-01-01T00:00:00Z");
        let aggregate = ScoreAggregate {
            count: 2,
            ..Default::default()
        };
        let summary = score_summary(
            ScoreDataType::Numeric,
            Some(&aggregate),
            from,
            DAY_NS,
            STATS_DAYS,
        );
        assert_eq!(summary.average, None);
        let summary = score_summary(
            ScoreDataType::Boolean,
            Some(&aggregate),
            from,
            DAY_NS,
            STATS_DAYS,
        );
        assert_eq!(summary.pass_rate, None);
    }

    #[test]
    fn score_summary_without_aggregate_is_empty_with_one_point_per_bucket() {
        let from = ns("2026-01-01T00:00:00Z");
        let summary = score_summary(
            ScoreDataType::Numeric,
            None,
            from,
            HOUR_NS,
            RULE_STATS_HOURS,
        );
        assert_eq!(summary.average, None);
        assert_eq!(summary.series.len(), RULE_STATS_HOURS);
        assert!(summary.series.iter().all(|point| point.value.is_none()));
        assert_eq!(summary.series[1].time, from + HOUR_NS);
    }

    #[test]
    fn rule_window_is_24_hours_ending_now() {
        let now = ns("2026-01-01T11:30:00Z");
        assert_eq!(
            rule_window(now),
            (ns("2025-12-31T12:00:00Z"), now),
            "the current hour and the 23 before it"
        );
        let (from, _) = rule_window(ns("2026-01-01T12:00:00Z"));
        assert_eq!(from, ns("2025-12-31T13:00:00Z"));
    }

    #[test]
    fn experiments_for_sorts_newest_first_then_id() {
        let id = |n: u128| Uuid::from_u128(n);
        let scored = vec![
            ScoredExperiment {
                name: "relevance".to_owned(),
                experiment_id: id(2),
                last_scored_at: 100,
            },
            ScoredExperiment {
                name: "relevance".to_owned(),
                experiment_id: id(1),
                last_scored_at: 100,
            },
            ScoredExperiment {
                name: "relevance".to_owned(),
                experiment_id: id(3),
                last_scored_at: 200,
            },
            ScoredExperiment {
                name: "other".to_owned(),
                experiment_id: id(4),
                last_scored_at: 300,
            },
        ];
        let order: Vec<Uuid> = experiments_for(&scored, "relevance")
            .into_iter()
            .map(|row| row.experiment_id)
            .collect();
        assert_eq!(order, vec![id(3), id(1), id(2)]);
    }

    #[test]
    fn ensure_complete_fails_when_truncated() {
        assert!(ensure_complete(true).is_err());
        assert!(ensure_complete(false).is_ok());
    }

    #[test]
    fn restrict_clause_lists_the_ids_as_placeholders() {
        let (clause, params) = restrict_clause("evaluator_id", None);
        assert!(clause.is_empty());
        assert!(params.is_empty());
        let ids = [Uuid::from_u128(1), Uuid::from_u128(2)];
        let (clause, params) = restrict_clause("evaluator_id", Some(&ids));
        assert_eq!(clause, " AND evaluator_id IN ({e0:String}, {e1:String})");
        assert_eq!(
            params.get("e1"),
            Some(&json!(Uuid::from_u128(2).to_string()))
        );
    }

    /// A window from the start of the day six days back to the clock, as the handler opens it.
    fn stats_window(app: &TestApp) -> (i64, i64) {
        window(app.state.clock.now_ns())
    }

    #[tokio::test]
    async fn read_scores_groups_by_evaluator_and_day() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(1_767_268_800_000_000_000);
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let (from, to) = stats_window(&app);
        let evaluator = Uuid::from_u128(0x0100);
        let other = Uuid::from_u128(0x0101);
        let at = |k: i64| now - 1 - k * DAY;
        let score = |name: &'static str, value: f64, evaluator_id, created_at| TestScore {
            name,
            value: ScoreValue::Numeric(value),
            evaluator_id,
            experiment_id: None,
            created_at,
            deleted: false,
            score_id: None,
            updated_at: None,
        };
        put_score(&app, project, score("x", 1.0, Some(evaluator), at(0))).await;
        put_score(&app, project, score("x", 0.5, Some(evaluator), at(2))).await;
        put_score(&app, project, score("x", 0.25, Some(evaluator), at(2))).await;
        put_score(&app, project, score("x", 9.0, Some(evaluator), at(7))).await;
        put_score(&app, project, score("x", 3.0, Some(other), at(0))).await;
        put_score(&app, project, score("x", 4.0, None, at(0))).await;
        // A live score and its tombstone (the same score id, written later): the tombstone hides it.
        let id = Uuid::from_u128(0x0200);
        put_score(
            &app,
            project,
            TestScore {
                score_id: Some(id),
                ..score("x", 6.0, Some(evaluator), at(1))
            },
        )
        .await;
        put_score(
            &app,
            project,
            TestScore {
                score_id: Some(id),
                deleted: true,
                updated_at: Some(now),
                ..score("x", 6.0, Some(evaluator), at(1))
            },
        )
        .await;

        let ctx = open_query_windowed(
            &app.state,
            seeded.org_id,
            project,
            Some(TimeWindow::new(from, to).unwrap()),
        )
        .await
        .unwrap();
        let aggregates = read_scores(
            &ctx,
            ScoreGroup::Evaluator,
            from,
            None,
            false,
            Deadline::start(),
        )
        .await
        .unwrap();
        let mine = aggregates.get(&evaluator).unwrap();
        assert_eq!(
            mine.count, 3,
            "the tombstone hides the live score it replaces"
        );
        assert_eq!(mine.numeric_count, 3);
        assert!((mine.numeric_sum - 1.75).abs() < 1e-9);
        assert_eq!(mine.buckets[6].0, 1);
        assert_eq!(mine.buckets[4].0, 2);
        assert!((mine.buckets[4].1 - 0.75).abs() < 1e-9);
        assert_eq!(aggregates.get(&other).map(|a| a.count), Some(1));
        assert_eq!(
            aggregates.len(),
            2,
            "the score without an evaluator is left out"
        );
    }

    #[tokio::test]
    async fn read_scores_restricts_to_the_given_ids() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(1_767_268_800_000_000_000);
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let (from, to) = stats_window(&app);
        let first = Uuid::from_u128(0x0300);
        let second = Uuid::from_u128(0x0301);
        for evaluator in [first, second] {
            put_score(
                &app,
                project,
                TestScore {
                    name: "x",
                    value: ScoreValue::Boolean(true),
                    evaluator_id: Some(evaluator),
                    experiment_id: None,
                    created_at: now - 1,
                    deleted: false,
                    score_id: None,
                    updated_at: None,
                },
            )
            .await;
        }
        let ctx = open_query_windowed(
            &app.state,
            seeded.org_id,
            project,
            Some(TimeWindow::new(from, to).unwrap()),
        )
        .await
        .unwrap();
        let aggregates = read_scores(
            &ctx,
            ScoreGroup::Evaluator,
            from,
            Some(&[second]),
            false,
            Deadline::start(),
        )
        .await
        .unwrap();
        assert_eq!(aggregates.len(), 1);
        assert_eq!(aggregates.get(&second).map(|a| a.count), Some(1));
    }

    #[tokio::test]
    async fn read_scores_top_category() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(1_767_268_800_000_000_000);
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let (from, to) = stats_window(&app);
        let first = Uuid::from_u128(0x0400);
        let second = Uuid::from_u128(0x0401);
        let label = |evaluator, text: &str| TestScore {
            name: "c",
            value: ScoreValue::Categorical {
                label: text.to_owned(),
                value: None,
            },
            evaluator_id: Some(evaluator),
            experiment_id: None,
            created_at: now - 1,
            deleted: false,
            score_id: None,
            updated_at: None,
        };
        for text in ["b", "a", "a"] {
            put_score(&app, project, label(first, text)).await;
        }
        for text in ["y", "x"] {
            put_score(&app, project, label(second, text)).await;
        }
        let ctx = open_query_windowed(
            &app.state,
            seeded.org_id,
            project,
            Some(TimeWindow::new(from, to).unwrap()),
        )
        .await
        .unwrap();
        let aggregates = read_scores(
            &ctx,
            ScoreGroup::Evaluator,
            from,
            None,
            true,
            Deadline::start(),
        )
        .await
        .unwrap();
        assert_eq!(
            aggregates.get(&first).unwrap().top_category,
            Some(("a".to_owned(), 2))
        );
        assert_eq!(
            aggregates.get(&second).unwrap().top_category,
            Some(("x".to_owned(), 1)),
            "a tie goes to the smaller label"
        );
    }

    #[tokio::test]
    async fn read_experiment_scores_groups_by_name_and_experiment() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(1_767_268_800_000_000_000);
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let first = Uuid::from_u128(0x0500);
        let second = Uuid::from_u128(0x0501);
        let old = Uuid::from_u128(0x0502);
        let score = |name: &'static str, experiment, created_at| TestScore {
            name,
            value: ScoreValue::Numeric(1.0),
            evaluator_id: None,
            experiment_id: Some(experiment),
            created_at,
            deleted: false,
            score_id: None,
            updated_at: None,
        };
        put_score(&app, project, score("relevance", first, now - DAY)).await;
        put_score(&app, project, score("relevance", first, now - 2 * DAY)).await;
        put_score(&app, project, score("relevance", second, now - 3 * DAY)).await;
        put_score(&app, project, score("toxicity", second, now - 3 * DAY)).await;
        put_score(&app, project, score("relevance", old, now - 31 * DAY)).await;

        let ctx = open_query_windowed(
            &app.state,
            seeded.org_id,
            project,
            Some(TimeWindow::new(now - EXPERIMENT_DAYS * DAY, i64::MAX).unwrap()),
        )
        .await
        .unwrap();
        let all = read_experiment_scores(&ctx, None, Deadline::start())
            .await
            .unwrap();
        assert_eq!(
            all.len(),
            3,
            "the 31-day-old experiment is outside the window"
        );
        let only = read_experiment_scores(&ctx, Some(&["relevance".to_owned()]), Deadline::start())
            .await
            .unwrap();
        assert_eq!(only.len(), 2);
        assert!(only.iter().all(|row| row.name == "relevance"));
        let first_row = only.iter().find(|row| row.experiment_id == first).unwrap();
        assert_eq!(first_row.last_scored_at, now - DAY);
    }
}
