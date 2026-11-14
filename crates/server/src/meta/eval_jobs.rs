//! `eval_jobs` rows: the durable evaluation queue (spec 04 § Evaluation; D-I-11, D-I-12,
//! D-I-28). `dedup_key` makes enqueues idempotent, `claims` fences a worker's finish, and
//! there is no lease: startup and the janitor re-queue interrupted `running` jobs.
#![cfg_attr(not(test), allow(dead_code))] // I3.6 to I3.9 call these.

use std::collections::HashMap;

use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use sqlx::query::Query;
use sqlx::sqlite::{Sqlite, SqliteArguments, SqlitePool, SqliteRow};
use sqlx::{FromRow, Row, SqliteConnection};
use uuid::Uuid;

use super::{MetaError, rule_spend};

/// A rule's job (D-I-11).
pub(crate) const KIND_RULE: &str = "rule";
/// A manual run from the evaluator page (D-I-22).
pub(crate) const KIND_MANUAL: &str = "manual";
/// A backfill job (M6).
pub(crate) const KIND_BACKFILL: &str = "backfill";

const KINDS: [&str; 3] = [KIND_RULE, KIND_MANUAL, KIND_BACKFILL];
const STATUSES: [&str; 5] = ["queued", "running", "done", "failed", "skipped"];

/// Interruptions a job may take before it fails (D-I-12).
const MAX_INTERRUPTIONS: i64 = 3;
/// A pruned or expired job's age limit for queued non-backfill jobs (D-I-28).
const DAY_NS: i64 = 86_400_000_000_000;

/// The columns `JobRow` reads, in the order `JobRecord` declares them. A macro so that the
/// queries stay string literals (sqlx takes static SQL only).
macro_rules! job_columns {
    () => {
        "id, project_id, kind, priority, rule_id, evaluator_id, variable_mapping, trace_id, \
         span_id, trace_start_ns, status, attempts, claims, load_retries, interruptions, \
         not_before, error_class, error, judge_trace_id, score_id, evaluator_version, \
         input_tokens, output_tokens, cost_usd, latency_ms, created_at, updated_at, finished_at"
    };
}

/// One stored job.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobRow {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// Owning project.
    pub(crate) project_id: ProjectId,
    /// `rule`, `manual` or `backfill`.
    pub(crate) kind: String,
    /// 0 manual, 1 rule, 2 backfill.
    pub(crate) priority: u8,
    /// The rule of a rule or backfill job.
    pub(crate) rule_id: Option<Uuid>,
    /// The evaluator the job runs.
    pub(crate) evaluator_id: Uuid,
    /// JSON object text of a manual run's mapping; rule jobs read the rule's.
    pub(crate) variable_mapping: Option<String>,
    /// The target trace.
    pub(crate) trace_id: TraceId,
    /// The target span, for a span rule or a span run.
    pub(crate) span_id: Option<SpanId>,
    /// Storage time hint, nanoseconds (D-I-23).
    pub(crate) trace_start_ns: i64,
    /// `queued`, `running`, `done`, `failed` or `skipped`.
    pub(crate) status: String,
    /// Calls made so far, given back by a pause.
    pub(crate) attempts: u32,
    /// Claims so far; the fence of `finish`.
    pub(crate) claims: u32,
    /// Trace-load retries so far.
    pub(crate) load_retries: u32,
    /// Restarts and janitor re-queues so far (D-I-12).
    pub(crate) interruptions: u32,
    /// The earliest time the job may be claimed, nanoseconds.
    pub(crate) not_before: i64,
    /// `retryable`, `permanent` or a skip reason.
    pub(crate) error_class: Option<String>,
    /// Free text about the last failure.
    pub(crate) error: Option<String>,
    /// The judge trace of the last call.
    pub(crate) judge_trace_id: Option<TraceId>,
    /// The evaluator score the job wrote.
    pub(crate) score_id: Option<Uuid>,
    /// The evaluator version the job ran.
    pub(crate) evaluator_version: Option<u32>,
    /// Input tokens of the last call.
    pub(crate) input_tokens: Option<u64>,
    /// Output tokens of the last call.
    pub(crate) output_tokens: Option<u64>,
    /// Cost of the last call, in USD.
    pub(crate) cost_usd: Option<f64>,
    /// Latency of the last call, in milliseconds.
    pub(crate) latency_ms: Option<u64>,
    /// Enqueue time, nanoseconds.
    pub(crate) created_at: i64,
    /// Last change time, nanoseconds.
    pub(crate) updated_at: i64,
    /// When the job reached a final status, nanoseconds.
    pub(crate) finished_at: Option<i64>,
}

/// A job to enqueue.
pub(crate) struct NewJob {
    /// Chosen by the caller.
    pub(crate) id: Uuid,
    /// Owning project.
    pub(crate) project_id: ProjectId,
    /// One of the `KIND_*` constants.
    pub(crate) kind: &'static str,
    /// The rule of a rule or backfill job.
    pub(crate) rule_id: Option<Uuid>,
    /// The evaluator the job runs.
    pub(crate) evaluator_id: Uuid,
    /// JSON object text of a manual run's mapping.
    pub(crate) variable_mapping: Option<String>,
    /// The target trace.
    pub(crate) trace_id: TraceId,
    /// The target span, when the job targets one.
    pub(crate) span_id: Option<SpanId>,
    /// Storage time hint, nanoseconds.
    pub(crate) trace_start_ns: i64,
}

/// One queued job the dispatcher may claim, with what its permits need.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    /// The job.
    pub(crate) job: JobRow,
    /// Stored kind name of the job's evaluator.
    pub(crate) evaluator_kind: String,
    /// The judge's connection, when the evaluator is a judge.
    pub(crate) connection_id: Option<Uuid>,
    /// `(max_concurrency, requests_per_minute, updated_at)` of the judge's connection.
    pub(crate) connection_limits: Option<(u32, u32, i64)>,
}

/// The token counts and cost of one finished call.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct JobUsage {
    /// Input tokens.
    pub(crate) input_tokens: Option<u64>,
    /// Output tokens.
    pub(crate) output_tokens: Option<u64>,
    /// Cost in USD.
    pub(crate) cost_usd: Option<f64>,
    /// Latency in milliseconds.
    pub(crate) latency_ms: Option<u64>,
}

/// How `finish` leaves the job. Every variant sets `updated_at = now`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Finish {
    /// `done`; sets `finished_at`, `judge_trace_id`, `score_id`, `evaluator_version`, the usage; clears `error_class`, `error`.
    Done {
        /// The judge trace of the run (D-I-16).
        judge_trace_id: TraceId,
        /// The score the run wrote.
        score_id: Uuid,
        /// The evaluator version the run used.
        evaluator_version: u32,
        /// The run's usage.
        usage: JobUsage,
    },
    /// `failed`; sets `finished_at`, `error_class`, `error`, and `judge_trace_id`, `evaluator_version`, the usage when given.
    Failed {
        /// `permanent` or `retryable`.
        class: &'static str,
        /// Free text about the failure.
        error: String,
        /// The judge trace, when the call made one.
        judge_trace_id: Option<TraceId>,
        /// The evaluator version, when known.
        evaluator_version: Option<u32>,
        /// The usage, when known.
        usage: JobUsage,
    },
    /// back to `queued` after a retryable failure; sets `not_before`, `error_class = 'retryable'`, `error`, `judge_trace_id`, the usage.
    Retry {
        /// The earliest next claim, nanoseconds.
        not_before: i64,
        /// Free text about the failure.
        error: String,
        /// The judge trace, when the call made one.
        judge_trace_id: Option<TraceId>,
        /// The usage of the call.
        usage: JobUsage,
    },
    /// back to `queued` with `load_retries += 1`, `not_before`, `error_class = 'retryable'`, `error`.
    LoadRetry {
        /// The earliest next claim, nanoseconds.
        not_before: i64,
        /// Why the trace did not load.
        error: &'static str,
    },
    /// back to `queued` with `not_before`, nothing else (a rule that is no longer runnable, a changed
    /// connection before the call, no trace-load permit).
    Requeue {
        /// The earliest next claim, nanoseconds.
        not_before: i64,
    },
    /// back to `queued` with `not_before` and `attempts = MAX(attempts - 1, 0)`: a pause gives back
    /// the attempt the run counted (D-I-14).
    PauseRequeue {
        /// The earliest next claim, nanoseconds.
        not_before: i64,
    },
    /// `skipped`; sets `finished_at`, `error`, clears `error_class`.
    Skipped {
        /// Why the job was skipped.
        error: &'static str,
    },
}

/// The filters of `list_jobs`. Each `None` matches every job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JobFilter {
    /// Jobs of this rule.
    pub(crate) rule_id: Option<Uuid>,
    /// Jobs of this evaluator.
    pub(crate) evaluator_id: Option<Uuid>,
    /// Jobs in this status.
    pub(crate) status: Option<&'static str>,
    /// Jobs of this kind.
    pub(crate) kind: Option<&'static str>,
}

/// Jobs per status of one rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JobCounts {
    /// Queued jobs.
    pub(crate) queued: u64,
    /// Running jobs.
    pub(crate) running: u64,
    /// Done jobs.
    pub(crate) done: u64,
    /// Failed jobs.
    pub(crate) failed: u64,
    /// Skipped jobs.
    pub(crate) skipped: u64,
}

/// Rows each step of `prune` changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PruneReport {
    /// Finished jobs deleted after 7 days.
    pub(crate) deleted: u64,
    /// Queued jobs skipped as `expired`.
    pub(crate) expired: u64,
    /// Jobs of inactive projects or orgs deleted.
    pub(crate) inactive: u64,
    /// Rule spend rows deleted.
    pub(crate) spend: u64,
}

/// The priority of a job kind: manual 0, rule 1, backfill 2 (D-I-6).
pub(crate) fn priority_of(kind: &str) -> u8 {
    match kind {
        KIND_MANUAL => 0,
        KIND_RULE => 1,
        _ => 2,
    }
}

/// The idempotency key (D-I-11): `rule:<rule_id>:<trace hex>:<span hex or ->` for rule and
/// backfill jobs, `manual:<id>` for manual runs.
pub(crate) fn dedup_key(job: &NewJob) -> String {
    if job.kind == KIND_MANUAL {
        return format!("manual:{}", job.id);
    }
    let rule = job
        .rule_id
        .map_or_else(|| "-".to_owned(), |id| id.to_string());
    let span = job
        .span_id
        .map_or_else(|| "-".to_owned(), |id| id.to_string());
    format!("rule:{rule}:{}:{span}", job.trace_id)
}

/// Enqueues a job as `queued`, due now. `false` means a job with the same dedup key exists.
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails.
pub(crate) async fn insert_job(
    conn: &mut SqliteConnection,
    job: NewJob,
    now: i64,
) -> Result<bool, MetaError> {
    let dedup = dedup_key(&job);
    let result = sqlx::query(
        "INSERT INTO eval_jobs \
         (id, project_id, kind, priority, rule_id, evaluator_id, variable_mapping, trace_id, \
         span_id, trace_start_ns, dedup_key, status, attempts, claims, load_retries, \
         interruptions, not_before, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', 0, 0, 0, 0, ?, ?, ?) \
         ON CONFLICT(dedup_key) DO NOTHING",
    )
    .bind(job.id.to_string())
    .bind(job.project_id.to_string())
    .bind(job.kind)
    .bind(i64::from(priority_of(job.kind)))
    .bind(job.rule_id.map(|id| id.to_string()))
    .bind(job.evaluator_id.to_string())
    .bind(job.variable_mapping)
    .bind(job.trace_id.as_bytes().to_vec())
    .bind(job.span_id.map(|span| span.as_bytes().to_vec()))
    .bind(job.trace_start_ns)
    .bind(dedup)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The job `id` in `project`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_job(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<JobRow>, MetaError> {
    let record: Option<JobRecord> = sqlx::query_as(concat!(
        "SELECT ",
        job_columns!(),
        " FROM eval_jobs WHERE project_id = ? AND id = ?"
    ))
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    record.map(row_from).transpose()
}

/// Jobs of `project`, newest first (`created_at DESC, id DESC`), strictly before `before` when
/// given, at most `limit` rows.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_jobs(
    pool: &SqlitePool,
    project: ProjectId,
    filter: &JobFilter,
    before: Option<(i64, Uuid)>,
    limit: i64,
) -> Result<Vec<JobRow>, MetaError> {
    let records: Vec<JobRecord> = sqlx::query_as(concat!(
        "SELECT ",
        job_columns!(),
        " FROM eval_jobs WHERE project_id = ?1 \
         AND (?2 IS NULL OR rule_id = ?2) \
         AND (?3 IS NULL OR evaluator_id = ?3) \
         AND (?4 IS NULL OR status = ?4) \
         AND (?5 IS NULL OR kind = ?5) \
         AND (?6 IS NULL OR created_at < ?6 OR (created_at = ?6 AND id < ?7)) \
         ORDER BY created_at DESC, id DESC LIMIT ?8"
    ))
    .bind(project.to_string())
    .bind(filter.rule_id.map(|id| id.to_string()))
    .bind(filter.evaluator_id.map(|id| id.to_string()))
    .bind(filter.status)
    .bind(filter.kind)
    .bind(before.map(|cursor| cursor.0))
    .bind(before.map(|cursor| cursor.1.to_string()))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    records.into_iter().map(row_from).collect()
}

/// The dispatcher's candidates (§ The candidate query): at most 20 per `(priority, project)`,
/// in `(priority, not_before, id)` order, skipping jobs whose rule is disabled or paused, whose
/// project or org is not active, and whose connection (or webhook evaluator) is in `saturated`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn candidates(
    pool: &SqlitePool,
    now: i64,
    saturated: &[Uuid],
    limit: i64,
) -> Result<Vec<Candidate>, MetaError> {
    let saturated: Vec<String> = saturated.iter().map(Uuid::to_string).collect();
    // A list of strings always serializes; the fallback is the empty list.
    let saturated = serde_json::to_string(&saturated).unwrap_or_else(|_| "[]".to_owned());
    let records: Vec<CandidateRecord> = sqlx::query_as(
        "SELECT * FROM (
           SELECT j.*, e.kind AS evaluator_kind, e.connection_id,
                  c.max_concurrency, c.requests_per_minute, c.updated_at AS connection_updated_at,
                  ROW_NUMBER() OVER (PARTITION BY j.priority, j.project_id ORDER BY j.not_before, j.id) AS rn
           FROM eval_jobs j
           JOIN evaluators e ON e.id = j.evaluator_id
           JOIN projects p ON p.id = j.project_id
           JOIN orgs o ON o.id = p.org_id
           LEFT JOIN online_rules r ON r.id = j.rule_id
           LEFT JOIN llm_connections c ON c.id = e.connection_id
           WHERE j.status = 'queued' AND j.not_before <= ?1
             AND p.state = 'active' AND o.state = 'active'
             AND (j.rule_id IS NULL OR (r.enabled = 1 AND r.paused_reason IS NULL))
             AND COALESCE(e.connection_id, e.id) NOT IN (SELECT value FROM json_each(?2))
         )
         WHERE rn <= 20
         ORDER BY priority, not_before, id
         LIMIT ?3",
    )
    .bind(now)
    .bind(saturated)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    records.into_iter().map(candidate_from).collect()
}

/// Claims a queued job: `queued → running`, `claims += 1`. Returns the new claim count, or
/// `None` when the job is not queued.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn claim(
    conn: &mut SqliteConnection,
    id: Uuid,
    now: i64,
) -> Result<Option<u32>, MetaError> {
    let claims: Option<i64> = sqlx::query_scalar(
        "UPDATE eval_jobs SET status = 'running', claims = claims + 1, updated_at = ? \
         WHERE id = ? AND status = 'queued' RETURNING claims",
    )
    .bind(now)
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    claims.map(|value| to_u32(value, "claims")).transpose()
}

/// Counts an attempt before the call: `attempts += 1`, for the running claim `claims`. Returns
/// the new attempt count, or `None` when the claim is stale.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn start_attempt(
    conn: &mut SqliteConnection,
    id: Uuid,
    claims: u32,
    now: i64,
) -> Result<Option<u32>, MetaError> {
    let attempts: Option<i64> = sqlx::query_scalar(
        "UPDATE eval_jobs SET attempts = attempts + 1, updated_at = ? \
         WHERE id = ? AND status = 'running' AND claims = ? RETURNING attempts",
    )
    .bind(now)
    .bind(id.to_string())
    .bind(i64::from(claims))
    .fetch_optional(&mut *conn)
    .await?;
    attempts.map(|value| to_u32(value, "attempts")).transpose()
}

/// Leaves a running job as `finish` says, fenced by `claims`. `false` means the claim is stale
/// (the job was re-claimed or deleted), and nothing changed (D-I-12).
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn finish(
    conn: &mut SqliteConnection,
    id: Uuid,
    claims: u32,
    finish: Finish,
    now: i64,
) -> Result<bool, MetaError> {
    let id = id.to_string();
    let claims = i64::from(claims);
    let result = match finish {
        Finish::Done {
            judge_trace_id,
            score_id,
            evaluator_version,
            usage,
        } => {
            let query = sqlx::query(
                "UPDATE eval_jobs SET status = 'done', finished_at = ?, updated_at = ?, \
                 judge_trace_id = ?, score_id = ?, evaluator_version = ?, input_tokens = ?, \
                 output_tokens = ?, cost_usd = ?, latency_ms = ?, error_class = NULL, error = NULL \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(now)
            .bind(now)
            .bind(trace_bytes(&judge_trace_id))
            .bind(score_id.to_string())
            .bind(i64::from(evaluator_version));
            bind_usage(query, usage)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
        }
        Finish::Failed {
            class,
            error,
            judge_trace_id,
            evaluator_version,
            usage,
        } => {
            let query = sqlx::query(
                "UPDATE eval_jobs SET status = 'failed', finished_at = ?, updated_at = ?, \
                 error_class = ?, error = ?, judge_trace_id = COALESCE(?, judge_trace_id), \
                 evaluator_version = COALESCE(?, evaluator_version), \
                 input_tokens = COALESCE(?, input_tokens), \
                 output_tokens = COALESCE(?, output_tokens), \
                 cost_usd = COALESCE(?, cost_usd), latency_ms = COALESCE(?, latency_ms) \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(now)
            .bind(now)
            .bind(class)
            .bind(error)
            .bind(judge_trace_id.as_ref().map(trace_bytes))
            .bind(evaluator_version.map(i64::from));
            bind_usage(query, usage)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
        }
        Finish::Retry {
            not_before,
            error,
            judge_trace_id,
            usage,
        } => {
            let query = sqlx::query(
                "UPDATE eval_jobs SET status = 'queued', not_before = ?, updated_at = ?, \
                 error_class = 'retryable', error = ?, judge_trace_id = ?, input_tokens = ?, \
                 output_tokens = ?, cost_usd = ?, latency_ms = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(not_before)
            .bind(now)
            .bind(error)
            .bind(judge_trace_id.as_ref().map(trace_bytes));
            bind_usage(query, usage)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
        }
        Finish::LoadRetry { not_before, error } => {
            sqlx::query(
                "UPDATE eval_jobs SET status = 'queued', load_retries = load_retries + 1, \
                 not_before = ?, updated_at = ?, error_class = 'retryable', error = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(not_before)
            .bind(now)
            .bind(error)
            .bind(id)
            .bind(claims)
            .execute(&mut *conn)
            .await?
        }
        Finish::Requeue { not_before } => {
            sqlx::query(
                "UPDATE eval_jobs SET status = 'queued', not_before = ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(not_before)
            .bind(now)
            .bind(id)
            .bind(claims)
            .execute(&mut *conn)
            .await?
        }
        Finish::PauseRequeue { not_before } => {
            sqlx::query(
                "UPDATE eval_jobs SET status = 'queued', not_before = ?, updated_at = ?, \
                 attempts = MAX(attempts - 1, 0) \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(not_before)
            .bind(now)
            .bind(id)
            .bind(claims)
            .execute(&mut *conn)
            .await?
        }
        Finish::Skipped { error } => {
            sqlx::query(
                "UPDATE eval_jobs SET status = 'skipped', finished_at = ?, updated_at = ?, \
                 error = ?, error_class = NULL \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(now)
            .bind(now)
            .bind(error)
            .bind(id)
            .bind(claims)
            .execute(&mut *conn)
            .await?
        }
    };
    Ok(result.rows_affected() > 0)
}

/// Skips the queued jobs of `rule_id` with `error`. Returns how many changed.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn skip_queued_of_rule(
    conn: &mut SqliteConnection,
    rule_id: Uuid,
    error: &'static str,
    now: i64,
) -> Result<u64, MetaError> {
    let result = sqlx::query(
        "UPDATE eval_jobs SET status = 'skipped', error = ?, finished_at = ?, updated_at = ? \
         WHERE rule_id = ? AND status = 'queued'",
    )
    .bind(error)
    .bind(now)
    .bind(now)
    .bind(rule_id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// The janitor: the interruption rule for `running` jobs whose `updated_at` is before
/// `older_than`. Returns `(requeued, failed)`.
///
/// # Errors
///
/// Returns [`MetaError`] when an update fails.
pub(crate) async fn requeue_stale(
    conn: &mut SqliteConnection,
    older_than: i64,
    now: i64,
) -> Result<(u64, u64), MetaError> {
    interrupt(conn, older_than, now).await
}

/// Startup: the interruption rule for every `running` job. Returns `(requeued, failed)`.
///
/// # Errors
///
/// Returns [`MetaError`] when an update fails.
pub(crate) async fn recover(
    conn: &mut SqliteConnection,
    now: i64,
) -> Result<(u64, u64), MetaError> {
    interrupt(conn, i64::MAX, now).await
}

/// The interruption rule (D-I-12), run inside the caller's write transaction: a `running` job
/// interrupted [`MAX_INTERRUPTIONS`] times fails, and the others go back to `queued` with one
/// more interruption. Only jobs with `updated_at < cutoff` are touched.
async fn interrupt(
    conn: &mut SqliteConnection,
    cutoff: i64,
    now: i64,
) -> Result<(u64, u64), MetaError> {
    let failed = sqlx::query(
        "UPDATE eval_jobs SET status = 'failed', error_class = 'permanent', \
         error = 'interrupted too often', finished_at = ?1, updated_at = ?1 \
         WHERE status = 'running' AND updated_at < ?2 AND interruptions >= ?3",
    )
    .bind(now)
    .bind(cutoff)
    .bind(MAX_INTERRUPTIONS)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let requeued = sqlx::query(
        "UPDATE eval_jobs SET status = 'queued', not_before = ?1, \
         interruptions = interruptions + 1, updated_at = ?1 \
         WHERE status = 'running' AND updated_at < ?2",
    )
    .bind(now)
    .bind(cutoff)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok((requeued, failed))
}

/// Deletes jobs of inactive projects and orgs, expires old queued jobs, deletes finished jobs
/// older than 7 days and prunes rule spend older than 62 days (D-I-28), in that order, in one
/// write transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when a statement fails.
pub(crate) async fn prune(conn: &mut SqliteConnection, now: i64) -> Result<PruneReport, MetaError> {
    let inactive = sqlx::query(
        "DELETE FROM eval_jobs WHERE project_id IN (\
           SELECT p.id FROM projects p JOIN orgs o ON o.id = p.org_id \
           WHERE p.state <> 'active' OR o.state <> 'active')",
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let day_ago = now.saturating_sub(DAY_NS);
    let week_ago = now.saturating_sub(7 * DAY_NS);
    let expired = sqlx::query(
        "UPDATE eval_jobs SET status = 'skipped', error = 'expired', finished_at = ?1, \
         updated_at = ?1 WHERE status = 'queued' AND \
         ((kind <> 'backfill' AND created_at < ?2) OR (kind = 'backfill' AND created_at < ?3))",
    )
    .bind(now)
    .bind(day_ago)
    .bind(week_ago)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let deleted = sqlx::query(
        "DELETE FROM eval_jobs WHERE finished_at IS NOT NULL AND finished_at < ? \
         AND status IN ('done', 'failed', 'skipped')",
    )
    .bind(week_ago)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let spend = rule_spend::prune_spend(conn, now.saturating_sub(62 * DAY_NS)).await?;
    Ok(PruneReport {
        deleted,
        expired,
        inactive,
        spend,
    })
}

/// Job counts per rule of `project` since `since` (nanoseconds), manual jobs excluded.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn counts_since(
    pool: &SqlitePool,
    project: ProjectId,
    since: i64,
) -> Result<HashMap<Uuid, JobCounts>, MetaError> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT rule_id, status, COUNT(*) FROM eval_jobs \
         WHERE project_id = ? AND rule_id IS NOT NULL AND created_at >= ? \
         GROUP BY rule_id, status",
    )
    .bind(project.to_string())
    .bind(since)
    .fetch_all(pool)
    .await?;
    let mut counts: HashMap<Uuid, JobCounts> = HashMap::new();
    for (rule, status, count) in rows {
        let rule = parse_uuid(&rule, "rule_id")?;
        let count = u64::try_from(count).map_err(|_| corrupt("status"))?;
        let entry = counts.entry(rule).or_default();
        match status.as_str() {
            "queued" => entry.queued += count,
            "running" => entry.running += count,
            "done" => entry.done += count,
            "failed" => entry.failed += count,
            "skipped" => entry.skipped += count,
            _ => return Err(corrupt("status")),
        }
    }
    Ok(counts)
}

/// Sets the bound values of `usage` on `query`, in the order the statements name them.
fn bind_usage<'q>(
    query: Query<'q, Sqlite, SqliteArguments>,
    usage: JobUsage,
) -> Query<'q, Sqlite, SqliteArguments> {
    query
        .bind(usage.input_tokens.map(to_i64))
        .bind(usage.output_tokens.map(to_i64))
        .bind(usage.cost_usd)
        .bind(usage.latency_ms.map(to_i64))
}

fn trace_bytes(trace: &TraceId) -> Vec<u8> {
    trace.as_bytes().to_vec()
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[derive(sqlx::FromRow)]
struct JobRecord {
    id: String,
    project_id: String,
    kind: String,
    priority: i64,
    rule_id: Option<String>,
    evaluator_id: String,
    variable_mapping: Option<String>,
    trace_id: Vec<u8>,
    span_id: Option<Vec<u8>>,
    trace_start_ns: i64,
    status: String,
    attempts: i64,
    claims: i64,
    load_retries: i64,
    interruptions: i64,
    not_before: i64,
    error_class: Option<String>,
    error: Option<String>,
    judge_trace_id: Option<Vec<u8>>,
    score_id: Option<String>,
    evaluator_version: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cost_usd: Option<f64>,
    latency_ms: Option<i64>,
    created_at: i64,
    updated_at: i64,
    finished_at: Option<i64>,
}

struct CandidateRecord {
    job: JobRecord,
    evaluator_kind: String,
    connection_id: Option<String>,
    max_concurrency: Option<i64>,
    requests_per_minute: Option<i64>,
    connection_updated_at: Option<i64>,
}

impl<'r> FromRow<'r, SqliteRow> for CandidateRecord {
    fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            job: JobRecord::from_row(row)?,
            evaluator_kind: row.try_get("evaluator_kind")?,
            connection_id: row.try_get("connection_id")?,
            max_concurrency: row.try_get("max_concurrency")?,
            requests_per_minute: row.try_get("requests_per_minute")?,
            connection_updated_at: row.try_get("connection_updated_at")?,
        })
    }
}

fn row_from(record: JobRecord) -> Result<JobRow, MetaError> {
    Ok(JobRow {
        id: parse_uuid(&record.id, "id")?,
        project_id: ProjectId::from_uuid(parse_uuid(&record.project_id, "project_id")?),
        kind: one_of(record.kind, &KINDS, "kind")?,
        priority: u8::try_from(record.priority).map_err(|_| corrupt("priority"))?,
        rule_id: optional_uuid(record.rule_id, "rule_id")?,
        evaluator_id: parse_uuid(&record.evaluator_id, "evaluator_id")?,
        variable_mapping: record.variable_mapping,
        trace_id: TraceId::from_slice(&record.trace_id).map_err(|_| corrupt("trace_id"))?,
        span_id: record
            .span_id
            .map(|bytes| SpanId::from_slice(&bytes).map_err(|_| corrupt("span_id")))
            .transpose()?,
        trace_start_ns: record.trace_start_ns,
        status: one_of(record.status, &STATUSES, "status")?,
        attempts: to_u32(record.attempts, "attempts")?,
        claims: to_u32(record.claims, "claims")?,
        load_retries: to_u32(record.load_retries, "load_retries")?,
        interruptions: to_u32(record.interruptions, "interruptions")?,
        not_before: record.not_before,
        error_class: record.error_class,
        error: record.error,
        judge_trace_id: record
            .judge_trace_id
            .map(|bytes| TraceId::from_slice(&bytes).map_err(|_| corrupt("judge_trace_id")))
            .transpose()?,
        score_id: optional_uuid(record.score_id, "score_id")?,
        evaluator_version: record
            .evaluator_version
            .map(|value| to_u32(value, "evaluator_version"))
            .transpose()?,
        input_tokens: record
            .input_tokens
            .map(|value| to_u64(value, "input_tokens"))
            .transpose()?,
        output_tokens: record
            .output_tokens
            .map(|value| to_u64(value, "output_tokens"))
            .transpose()?,
        cost_usd: record.cost_usd,
        latency_ms: record
            .latency_ms
            .map(|value| to_u64(value, "latency_ms"))
            .transpose()?,
        created_at: record.created_at,
        updated_at: record.updated_at,
        finished_at: record.finished_at,
    })
}

fn candidate_from(record: CandidateRecord) -> Result<Candidate, MetaError> {
    let connection_limits = match (
        record.max_concurrency,
        record.requests_per_minute,
        record.connection_updated_at,
    ) {
        (Some(concurrency), Some(rpm), Some(updated_at)) => Some((
            to_u32(concurrency, "max_concurrency")?,
            to_u32(rpm, "requests_per_minute")?,
            updated_at,
        )),
        _ => None,
    };
    Ok(Candidate {
        job: row_from(record.job)?,
        evaluator_kind: record.evaluator_kind,
        connection_id: optional_uuid(record.connection_id, "connection_id")?,
        connection_limits,
    })
}

fn one_of(value: String, allowed: &[&str], column: &'static str) -> Result<String, MetaError> {
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(corrupt(column))
    }
}

fn to_u32(value: i64, column: &'static str) -> Result<u32, MetaError> {
    u32::try_from(value).map_err(|_| corrupt(column))
}

fn to_u64(value: i64, column: &'static str) -> Result<u64, MetaError> {
    u64::try_from(value).map_err(|_| corrupt(column))
}

fn corrupt(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "eval_jobs",
        column,
    }
}

fn parse_uuid(value: &str, column: &'static str) -> Result<Uuid, MetaError> {
    value.parse().map_err(|_| corrupt(column))
}

fn optional_uuid(value: Option<String>, column: &'static str) -> Result<Option<Uuid>, MetaError> {
    value.map(|text| parse_uuid(&text, column)).transpose()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use llmobs_core::ids::{OrgId, ProjectId, SpanId, TraceId};
    use uuid::Uuid;

    use super::{
        Candidate, Finish, JobFilter, JobRow, JobUsage, KIND_BACKFILL, KIND_MANUAL, KIND_RULE,
        NewJob, candidates, claim, counts_since, dedup_key, finish, get_job, insert_job, list_jobs,
        prune, recover, requeue_stale, skip_queued_of_rule, start_attempt,
    };
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::online_rules::{NewRule, PAUSE_ERRORS, RuleFields, insert_rule, pause_rule};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::rule_spend::add_spend;
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000_000_000_000_000_000;
    const DAY: i64 = 86_400_000_000_000;

    struct Seed {
        org: OrgId,
        project: ProjectId,
        connection: Uuid,
        judge: Uuid,
        webhook: Uuid,
        rule: Uuid,
    }

    async fn seed(db: &MetaDb) -> Seed {
        db.write(|conn| {
            Box::pin(async move {
                let org = insert_org(conn, "Acme", "acme", NOW).await?.id;
                let project = insert_project(conn, org, "Support", "support", None, NOW)
                    .await?
                    .id;
                let connection = Uuid::now_v7();
                insert_connection(conn, connection, org, connection_fields(), None, NOW).await?;
                let judge = Uuid::now_v7();
                insert_evaluator(
                    conn,
                    judge,
                    project,
                    "llm_judge",
                    evaluator_fields("relevance", Some(connection)),
                    None,
                    NOW,
                )
                .await?;
                let webhook = Uuid::now_v7();
                insert_evaluator(
                    conn,
                    webhook,
                    project,
                    "webhook",
                    evaluator_fields("hook", None),
                    None,
                    NOW,
                )
                .await?;
                let rule = Uuid::now_v7();
                insert_rule(
                    conn,
                    NewRule {
                        id: rule,
                        project_id: project,
                        evaluator_id: judge,
                        target: "trace".to_owned(),
                        created_by: None,
                    },
                    RuleFields {
                        name: "rule".to_owned(),
                        filter: None,
                        variable_mapping: "{}".to_owned(),
                        sample_rate: 1.0,
                        budget_usd: None,
                        budget_period: None,
                        enabled: true,
                    },
                    NOW,
                )
                .await?;
                Ok::<Seed, MetaError>(Seed {
                    org,
                    project,
                    connection,
                    judge,
                    webhook,
                    rule,
                })
            })
        })
        .await
        .unwrap()
    }

    fn connection_fields() -> ConnectionFields {
        ConnectionFields {
            name: "main".to_owned(),
            adapter: "openai".to_owned(),
            base_url: None,
            secret_enc: vec![1],
            api_key_hint: "WXYZ".to_owned(),
            extra_headers_enc: None,
            extra_header_names: Vec::new(),
            default_model: None,
            structured_output: "native".to_owned(),
            max_concurrency: 4,
            requests_per_minute: 300,
        }
    }

    fn evaluator_fields(name: &str, connection: Option<Uuid>) -> EvaluatorFields {
        EvaluatorFields {
            name: name.to_owned(),
            config: r#"{"model":"gpt-5-mini"}"#.to_owned(),
            output_score: r#"{"name":"relevance","data_type":"numeric"}"#.to_owned(),
            connection_id: connection,
            secret_enc: None,
            version: 1,
        }
    }

    fn trace(hex: &str) -> TraceId {
        TraceId::from_str(hex).unwrap()
    }

    fn rule_job(seeded: &Seed, trace_id: TraceId, span: Option<SpanId>) -> NewJob {
        NewJob {
            id: Uuid::now_v7(),
            project_id: seeded.project,
            kind: KIND_RULE,
            rule_id: Some(seeded.rule),
            evaluator_id: seeded.judge,
            variable_mapping: None,
            trace_id,
            span_id: span,
            trace_start_ns: NOW,
        }
    }

    fn manual_job(seeded: &Seed, trace_id: TraceId, evaluator: Uuid) -> NewJob {
        NewJob {
            id: Uuid::now_v7(),
            project_id: seeded.project,
            kind: KIND_MANUAL,
            rule_id: None,
            evaluator_id: evaluator,
            variable_mapping: Some(r#"{"input":"trace.input"}"#.to_owned()),
            trace_id,
            span_id: None,
            trace_start_ns: NOW,
        }
    }

    async fn enqueue(db: &MetaDb, job: NewJob, now: i64) -> bool {
        db.write(move |conn| Box::pin(async move { insert_job(conn, job, now).await }))
            .await
            .unwrap()
    }

    async fn read(db: &MetaDb, project: ProjectId, id: Uuid) -> JobRow {
        get_job(db.reader(), project, id).await.unwrap().unwrap()
    }

    async fn claim_job(db: &MetaDb, id: Uuid, now: i64) -> Option<u32> {
        db.write(move |conn| Box::pin(async move { claim(conn, id, now).await }))
            .await
            .unwrap()
    }

    async fn attempt(db: &MetaDb, id: Uuid, claims: u32, now: i64) -> Option<u32> {
        db.write(move |conn| Box::pin(async move { start_attempt(conn, id, claims, now).await }))
            .await
            .unwrap()
    }

    async fn finish_job(db: &MetaDb, id: Uuid, claims: u32, how: Finish, now: i64) -> bool {
        db.write(move |conn| Box::pin(async move { finish(conn, id, claims, how, now).await }))
            .await
            .unwrap()
    }

    /// Enqueues a rule job, claims it and counts one attempt: `(id, claims)`.
    async fn running_job(db: &MetaDb, seeded: &Seed, hex: &str) -> (Uuid, u32) {
        let job = rule_job(seeded, trace(hex), None);
        let id = job.id;
        assert!(enqueue(db, job, NOW).await);
        let claims = claim_job(db, id, NOW).await.unwrap();
        assert_eq!(attempt(db, id, claims, NOW).await, Some(1));
        (id, claims)
    }

    /// Sets a job's columns with raw SQL, for states the functions cannot reach directly.
    async fn raw(db: &MetaDb, sql: &'static str, id: Uuid) {
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query(sql)
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dedup_key_formats_follow_the_kind() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let span = SpanId::from_str("b100000000000071").unwrap();
        let job = rule_job(
            &seeded,
            trace("a1000000000000000000000000000001"),
            Some(span),
        );
        assert_eq!(
            dedup_key(&job),
            format!(
                "rule:{}:a1000000000000000000000000000001:b100000000000071",
                seeded.rule
            )
        );
        let manual = manual_job(
            &seeded,
            trace("a1000000000000000000000000000001"),
            seeded.judge,
        );
        assert_eq!(dedup_key(&manual), format!("manual:{}", manual.id));
    }

    #[tokio::test]
    async fn the_same_rule_and_target_enqueue_once_and_manual_runs_never_collide() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let target = trace("a1000000000000000000000000000001");
        assert!(enqueue(&db, rule_job(&seeded, target, None), NOW).await);
        assert!(!enqueue(&db, rule_job(&seeded, target, None), NOW).await);
        assert!(enqueue(&db, manual_job(&seeded, target, seeded.judge), NOW).await);
        assert!(enqueue(&db, manual_job(&seeded, target, seeded.judge), NOW).await);
    }

    #[tokio::test]
    async fn claim_is_once_and_a_stale_attempt_is_refused() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let job = rule_job(&seeded, trace("a1000000000000000000000000000001"), None);
        let id = job.id;
        enqueue(&db, job, NOW).await;
        assert_eq!(claim_job(&db, id, NOW).await, Some(1));
        assert_eq!(claim_job(&db, id, NOW).await, None);
        assert_eq!(attempt(&db, id, 0, NOW).await, None, "a stale claim");
        assert_eq!(attempt(&db, id, 1, NOW).await, Some(1));
        let row = read(&db, seeded.project, id).await;
        assert_eq!(
            (row.status.as_str(), row.claims, row.attempts),
            ("running", 1, 1)
        );
    }

    #[tokio::test]
    async fn finish_with_a_stale_claim_changes_nothing() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let (id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000001").await;
        let before = read(&db, seeded.project, id).await;
        let changed = finish_job(
            &db,
            id,
            claims + 1,
            Finish::Skipped { error: "late" },
            NOW + 1,
        )
        .await;
        assert!(!changed);
        assert_eq!(read(&db, seeded.project, id).await, before);
    }

    #[tokio::test]
    async fn each_finish_variant_sets_its_columns() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let project = seeded.project;
        let judge_trace = trace("e1000000000000000000000000000002");
        let usage = JobUsage {
            input_tokens: Some(120),
            output_tokens: Some(8),
            cost_usd: Some(0.002),
            latency_ms: Some(340),
        };
        let later = NOW + 5;

        // done
        let (id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000011").await;
        let score = Uuid::now_v7();
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::Done {
                    judge_trace_id: judge_trace,
                    score_id: score,
                    evaluator_version: 2,
                    usage,
                },
                later,
            )
            .await
        );
        let row = read(&db, project, id).await;
        assert_eq!(row.status, "done");
        assert_eq!(row.finished_at, Some(later));
        assert_eq!(row.judge_trace_id, Some(judge_trace));
        assert_eq!(row.score_id, Some(score));
        assert_eq!(row.evaluator_version, Some(2));
        assert_eq!((row.input_tokens, row.output_tokens), (Some(120), Some(8)));
        assert_eq!(row.cost_usd, Some(0.002));
        assert_eq!(row.latency_ms, Some(340));
        assert_eq!((row.error_class, row.error), (None, None));
        assert_eq!(row.updated_at, later);

        // failed keeps the usage it was not given
        let (id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000012").await;
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::Failed {
                    class: "permanent",
                    error: "bad output".to_owned(),
                    judge_trace_id: None,
                    evaluator_version: None,
                    usage: JobUsage::default(),
                },
                later,
            )
            .await
        );
        let row = read(&db, project, id).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_class.as_deref(), Some("permanent"));
        assert_eq!(row.error.as_deref(), Some("bad output"));
        assert_eq!(row.finished_at, Some(later));
        assert_eq!(row.judge_trace_id, None);

        // retry goes back to queued with the backoff, keeping the attempt
        let (id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000013").await;
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::Retry {
                    not_before: later + 30,
                    error: "provider status 503".to_owned(),
                    judge_trace_id: None,
                    usage: JobUsage::default(),
                },
                later,
            )
            .await
        );
        let row = read(&db, project, id).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.not_before, later + 30);
        assert_eq!(row.error_class.as_deref(), Some("retryable"));
        assert_eq!(row.error.as_deref(), Some("provider status 503"));
        assert_eq!(row.attempts, 1);
        assert_eq!(row.finished_at, None);

        // load retry counts the load
        let (id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000014").await;
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::LoadRetry {
                    not_before: later + 30,
                    error: "timeout",
                },
                later,
            )
            .await
        );
        let row = read(&db, project, id).await;
        assert_eq!((row.status.as_str(), row.load_retries), ("queued", 1));
        assert_eq!(row.error_class.as_deref(), Some("retryable"));
        assert_eq!(row.error.as_deref(), Some("timeout"));

        // requeue touches only the schedule
        let (id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000015").await;
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::Requeue {
                    not_before: later + 9
                },
                later
            )
            .await
        );
        let row = read(&db, project, id).await;
        assert_eq!(
            (row.status.as_str(), row.not_before, row.attempts),
            ("queued", later + 9, 1)
        );
        assert_eq!((row.error_class, row.error), (None, None));

        // skipped
        let (id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000016").await;
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::Skipped {
                    error: "rule disabled"
                },
                later
            )
            .await
        );
        let row = read(&db, project, id).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.finished_at, Some(later));
        assert_eq!(row.error.as_deref(), Some("rule disabled"));
    }

    #[tokio::test]
    async fn pause_requeue_gives_back_the_attempt_and_never_goes_below_zero() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let job = rule_job(&seeded, trace("a1000000000000000000000000000021"), None);
        let id = job.id;
        enqueue(&db, job, NOW).await;
        let claims = claim_job(&db, id, NOW).await.unwrap();
        assert_eq!(attempt(&db, id, claims, NOW).await, Some(1));
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::PauseRequeue {
                    not_before: NOW + 1
                },
                NOW
            )
            .await
        );
        let row = read(&db, seeded.project, id).await;
        assert_eq!((row.status.as_str(), row.attempts), ("queued", 0));

        let claims = claim_job(&db, id, NOW).await.unwrap();
        assert_eq!(claims, 2);
        assert!(
            finish_job(
                &db,
                id,
                claims,
                Finish::PauseRequeue {
                    not_before: NOW + 2
                },
                NOW
            )
            .await
        );
        let row = read(&db, seeded.project, id).await;
        assert_eq!(row.attempts, 0, "a pause never takes attempts below 0");
    }

    #[tokio::test]
    async fn candidates_follow_priority_and_the_per_project_cap() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let org = seeded.org;
        let other = db
            .write(move |conn| {
                Box::pin(async move {
                    Ok::<ProjectId, MetaError>(
                        insert_project(conn, org, "Other", "other", None, NOW)
                            .await?
                            .id,
                    )
                })
            })
            .await
            .unwrap();
        for index in 0..25 {
            let hex = format!("{:032x}", 0x1000 + index);
            enqueue(&db, manual_job(&seeded, trace(&hex), seeded.judge), NOW).await;
        }
        let mut of_other = manual_job(
            &seeded,
            trace("a2000000000000000000000000000001"),
            seeded.judge,
        );
        of_other.project_id = other;
        let other_id = of_other.id;
        enqueue(&db, of_other, NOW).await;
        let rule_id = rule_job(&seeded, trace("a3000000000000000000000000000001"), None);
        let rule_queued = rule_id.id;
        enqueue(&db, rule_id, NOW).await;

        let rows = candidates(db.reader(), NOW + 1, &[], 100).await.unwrap();
        let manual_rows: Vec<&Candidate> = rows
            .iter()
            .filter(|row| row.job.kind == KIND_MANUAL)
            .collect();
        assert_eq!(
            manual_rows.len(),
            21,
            "20 of the first project and 1 of the other"
        );
        assert!(manual_rows.iter().any(|row| row.job.id == other_id));
        assert_eq!(
            rows[0].job.kind, KIND_MANUAL,
            "manual runs have the highest priority"
        );
        assert_eq!(rows.last().unwrap().job.id, rule_queued);
        assert_eq!(rows.last().unwrap().job.priority, 1);
        assert_eq!(rows[0].job.priority, 0);
        assert_eq!(rows.last().unwrap().connection_id, Some(seeded.connection));
        let (concurrency, rpm, _) = rows.last().unwrap().connection_limits.unwrap();
        assert_eq!((concurrency, rpm), (4, 300));
        assert_eq!(rows.last().unwrap().evaluator_kind, "llm_judge");

        let not_yet = candidates(db.reader(), NOW - 1, &[], 100).await.unwrap();
        assert!(not_yet.is_empty(), "a job due later is not a candidate");
    }

    #[tokio::test]
    async fn candidates_skip_saturated_connections_webhooks_paused_rules_and_deleting_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let judge_manual = manual_job(
            &seeded,
            trace("a1000000000000000000000000000031"),
            seeded.judge,
        );
        let judge_manual_id = judge_manual.id;
        enqueue(&db, judge_manual, NOW).await;
        let hook = manual_job(
            &seeded,
            trace("a1000000000000000000000000000032"),
            seeded.webhook,
        );
        let hook_id = hook.id;
        enqueue(&db, hook, NOW).await;
        let rule_job_one = rule_job(&seeded, trace("a1000000000000000000000000000033"), None);
        let rule_one = rule_job_one.id;
        enqueue(&db, rule_job_one, NOW).await;

        let saturated = candidates(db.reader(), NOW, &[seeded.connection, seeded.webhook], 100)
            .await
            .unwrap();
        assert!(
            saturated.is_empty(),
            "the judge's connection and the webhook are saturated"
        );

        let free = candidates(db.reader(), NOW, &[], 100).await.unwrap();
        assert_eq!(free.len(), 3);

        let rule = seeded.rule;
        db.write(move |conn| {
            Box::pin(async move {
                pause_rule(conn, rule, PAUSE_ERRORS, None, NOW)
                    .await
                    .map(|_| ())
            })
        })
        .await
        .unwrap();
        let paused = candidates(db.reader(), NOW, &[], 100).await.unwrap();
        let ids: Vec<Uuid> = paused.iter().map(|row| row.job.id).collect();
        assert!(!ids.contains(&rule_one), "a paused rule's job is excluded");
        assert!(
            ids.contains(&judge_manual_id),
            "manual runs are not excluded by a rule"
        );
        assert!(ids.contains(&hook_id));

        raw(
            &db,
            "UPDATE projects SET state = 'deleting' WHERE id = ?",
            seeded.project.as_uuid(),
        )
        .await;
        let deleting = candidates(db.reader(), NOW, &[], 100).await.unwrap();
        assert!(
            deleting.is_empty(),
            "a deleting project's jobs are excluded"
        );
    }

    #[tokio::test]
    async fn recover_fails_a_job_interrupted_three_times_and_requeues_the_rest() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let (failed_id, _) = running_job(&db, &seeded, "a1000000000000000000000000000041").await;
        raw(
            &db,
            "UPDATE eval_jobs SET interruptions = 3 WHERE id = ?",
            failed_id,
        )
        .await;
        let (requeued_id, _) = running_job(&db, &seeded, "a1000000000000000000000000000042").await;
        raw(
            &db,
            "UPDATE eval_jobs SET interruptions = 2, claims = 6 WHERE id = ?",
            requeued_id,
        )
        .await;

        let (requeued, failed) = db
            .write(move |conn| Box::pin(async move { recover(conn, NOW + 10).await }))
            .await
            .unwrap();
        assert_eq!((requeued, failed), (1, 1));
        let failed_row = read(&db, seeded.project, failed_id).await;
        assert_eq!(failed_row.status, "failed");
        assert_eq!(failed_row.error.as_deref(), Some("interrupted too often"));
        assert_eq!(failed_row.error_class.as_deref(), Some("permanent"));
        let requeued_row = read(&db, seeded.project, requeued_id).await;
        assert_eq!(
            (requeued_row.status.as_str(), requeued_row.interruptions),
            ("queued", 3)
        );
        assert_eq!(requeued_row.claims, 6, "claims do not count");
    }

    #[tokio::test]
    async fn requeue_stale_touches_only_old_running_rows() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let (old_id, _) = running_job(&db, &seeded, "a1000000000000000000000000000051").await;
        raw(
            &db,
            "UPDATE eval_jobs SET updated_at = 100 WHERE id = ?",
            old_id,
        )
        .await;
        let (fresh_id, _) = running_job(&db, &seeded, "a1000000000000000000000000000052").await;
        raw(
            &db,
            "UPDATE eval_jobs SET updated_at = 900 WHERE id = ?",
            fresh_id,
        )
        .await;
        let (given_up_id, _) = running_job(&db, &seeded, "a1000000000000000000000000000053").await;
        raw(
            &db,
            "UPDATE eval_jobs SET updated_at = 100, interruptions = 3 WHERE id = ?",
            given_up_id,
        )
        .await;

        let (requeued, failed) = db
            .write(move |conn| Box::pin(async move { requeue_stale(conn, 500, 1_000).await }))
            .await
            .unwrap();
        assert_eq!((requeued, failed), (1, 1));
        let old = read(&db, seeded.project, old_id).await;
        assert_eq!((old.status.as_str(), old.interruptions), ("queued", 1));
        let fresh = read(&db, seeded.project, fresh_id).await;
        assert_eq!(fresh.status, "running");
        let given_up = read(&db, seeded.project, given_up_id).await;
        assert_eq!(given_up.status, "failed");
    }

    #[tokio::test]
    async fn skip_queued_of_rule_skips_only_that_rules_queued_jobs() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let queued = rule_job(&seeded, trace("a1000000000000000000000000000061"), None);
        let queued_id = queued.id;
        enqueue(&db, queued, NOW).await;
        let (running_id, _) = running_job(&db, &seeded, "a1000000000000000000000000000062").await;
        let rule = seeded.rule;
        let changed = db
            .write(move |conn| {
                Box::pin(async move { skip_queued_of_rule(conn, rule, "rule paused", NOW).await })
            })
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let row = read(&db, seeded.project, queued_id).await;
        assert_eq!(
            (row.status.as_str(), row.error.as_deref()),
            ("skipped", Some("rule paused"))
        );
        assert_eq!(
            read(&db, seeded.project, running_id).await.status,
            "running"
        );
    }

    #[tokio::test]
    async fn list_jobs_filters_each_field_and_pages_newest_first() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let project = seeded.project;
        let other_evaluator = db
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        id,
                        project,
                        "webhook",
                        evaluator_fields("other", None),
                        None,
                        NOW,
                    )
                    .await?;
                    Ok::<Uuid, MetaError>(id)
                })
            })
            .await
            .unwrap();
        let rule_one = rule_job(&seeded, trace("a1000000000000000000000000000071"), None);
        let rule_id = rule_one.id;
        enqueue(&db, rule_one, NOW).await;
        let manual = manual_job(
            &seeded,
            trace("a1000000000000000000000000000072"),
            seeded.judge,
        );
        let manual_id = manual.id;
        enqueue(&db, manual, NOW + 1).await;
        let hook = manual_job(
            &seeded,
            trace("a1000000000000000000000000000073"),
            other_evaluator,
        );
        let hook_id = hook.id;
        enqueue(&db, hook, NOW + 2).await;
        claim_job(&db, manual_id, NOW + 3).await;

        let ids = |rows: Vec<JobRow>| -> Vec<Uuid> { rows.into_iter().map(|row| row.id).collect() };
        let all = list_jobs(db.reader(), project, &JobFilter::default(), None, 10)
            .await
            .unwrap();
        assert_eq!(ids(all), vec![hook_id, manual_id, rule_id], "newest first");

        let by_rule = list_jobs(
            db.reader(),
            project,
            &JobFilter {
                rule_id: Some(seeded.rule),
                ..Default::default()
            },
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(ids(by_rule), vec![rule_id]);
        let by_evaluator = list_jobs(
            db.reader(),
            project,
            &JobFilter {
                evaluator_id: Some(other_evaluator),
                ..Default::default()
            },
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(ids(by_evaluator), vec![hook_id]);
        let by_status = list_jobs(
            db.reader(),
            project,
            &JobFilter {
                status: Some("running"),
                ..Default::default()
            },
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(ids(by_status), vec![manual_id]);
        let by_kind = list_jobs(
            db.reader(),
            project,
            &JobFilter {
                kind: Some(KIND_RULE),
                ..Default::default()
            },
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(ids(by_kind), vec![rule_id]);

        let first = list_jobs(db.reader(), project, &JobFilter::default(), None, 1)
            .await
            .unwrap();
        let cursor = (first[0].created_at, first[0].id);
        let second = list_jobs(db.reader(), project, &JobFilter::default(), Some(cursor), 1)
            .await
            .unwrap();
        assert_eq!(ids(second.clone()), vec![manual_id]);
        let cursor = (second[0].created_at, second[0].id);
        let third = list_jobs(db.reader(), project, &JobFilter::default(), Some(cursor), 1)
            .await
            .unwrap();
        assert_eq!(ids(third), vec![rule_id]);
    }

    #[tokio::test]
    async fn list_jobs_breaks_created_at_ties_by_id_descending() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let first = manual_job(
            &seeded,
            trace("a1000000000000000000000000000081"),
            seeded.judge,
        );
        let second = manual_job(
            &seeded,
            trace("a1000000000000000000000000000082"),
            seeded.judge,
        );
        let (low, high) = if first.id < second.id {
            (first.id, second.id)
        } else {
            (second.id, first.id)
        };
        enqueue(&db, first, NOW).await;
        enqueue(&db, second, NOW).await;
        let rows = list_jobs(db.reader(), seeded.project, &JobFilter::default(), None, 10)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![high, low]
        );
    }

    #[tokio::test]
    async fn prune_runs_each_step() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let project = seeded.project;

        // expired: a queued manual run older than a day, and a backfill job that is not yet.
        let manual = manual_job(
            &seeded,
            trace("a1000000000000000000000000000091"),
            seeded.judge,
        );
        let manual_id = manual.id;
        enqueue(&db, manual, NOW).await;
        let mut backfill = manual_job(
            &seeded,
            trace("a1000000000000000000000000000092"),
            seeded.judge,
        );
        backfill.kind = KIND_BACKFILL;
        backfill.rule_id = Some(seeded.rule);
        backfill.variable_mapping = None;
        let backfill_id = backfill.id;
        enqueue(&db, backfill, NOW).await;

        // deleted: a done job finished 8 days before the prune.
        let (done_id, claims) = running_job(&db, &seeded, "a1000000000000000000000000000093").await;
        finish_job(
            &db,
            done_id,
            claims,
            Finish::Skipped { error: "old" },
            NOW - 8 * DAY,
        )
        .await;

        let at = NOW + 25 * 3_600_000_000_000;
        // At NOW + 25 h the manual run is past 24 h and the 8-day-old finish is past 7 days.
        let report = db
            .write(move |conn| Box::pin(async move { prune(conn, at).await }))
            .await
            .unwrap();
        assert_eq!(report.expired, 1, "only the manual run is past 24 h");
        assert_eq!(report.deleted, 1, "the finish 8 days ago is past 7 days");
        assert_eq!(report.inactive, 0);
        assert!(
            get_job(db.reader(), project, done_id)
                .await
                .unwrap()
                .is_none()
        );
        let manual_row = read(&db, project, manual_id).await;
        assert_eq!(
            (manual_row.status.as_str(), manual_row.error.as_deref()),
            ("skipped", Some("expired"))
        );
        assert_eq!(read(&db, project, backfill_id).await.status, "queued");

        // At NOW + 8 days the backfill job is past 7 days too.
        let later = NOW + 8 * DAY;
        let report = db
            .write(move |conn| Box::pin(async move { prune(conn, later).await }))
            .await
            .unwrap();
        assert_eq!(report.expired, 1, "the backfill job is past 7 days");
        assert_eq!(read(&db, project, backfill_id).await.status, "skipped");

        // inactive: a deleting project's jobs go.
        raw(
            &db,
            "UPDATE projects SET state = 'deleting' WHERE id = ?",
            project.as_uuid(),
        )
        .await;
        let report = db
            .write(move |conn| Box::pin(async move { prune(conn, later).await }))
            .await
            .unwrap();
        assert!(report.inactive >= 2, "{report:?}");
        assert!(
            get_job(db.reader(), project, manual_id)
                .await
                .unwrap()
                .is_none()
        );

        // spend: a day row older than 62 days goes.
        let spend_rule = seeded.rule;
        db.write(move |conn| {
            Box::pin(async move { add_spend(conn, spend_rule, NOW - 70 * DAY, 1.0).await })
        })
        .await
        .unwrap();
        let report = db
            .write(move |conn| Box::pin(async move { prune(conn, NOW).await }))
            .await
            .unwrap();
        assert_eq!(report.spend, 1);
    }

    #[tokio::test]
    async fn counts_since_groups_by_rule_and_status_and_ignores_manual_runs() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let queued = rule_job(&seeded, trace("a10000000000000000000000000000a1"), None);
        enqueue(&db, queued, NOW).await;
        let (done_id, claims) = running_job(&db, &seeded, "a10000000000000000000000000000a2").await;
        finish_job(&db, done_id, claims, Finish::Skipped { error: "x" }, NOW).await;
        let old = rule_job(&seeded, trace("a10000000000000000000000000000a3"), None);
        enqueue(&db, old, NOW - 10 * DAY).await;
        enqueue(
            &db,
            manual_job(
                &seeded,
                trace("a10000000000000000000000000000a4"),
                seeded.judge,
            ),
            NOW,
        )
        .await;

        let counts = counts_since(db.reader(), seeded.project, NOW - DAY)
            .await
            .unwrap();
        let mine = counts.get(&seeded.rule).copied().unwrap();
        assert_eq!(mine.queued, 1);
        assert_eq!(mine.skipped, 1);
        assert_eq!(mine.running, 0);
        assert_eq!(counts.len(), 1);
    }

    #[tokio::test]
    async fn deleting_the_rule_or_the_evaluator_cascades_its_jobs() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let rule_job_one = rule_job(&seeded, trace("a10000000000000000000000000000b1"), None);
        let rule_job_id = rule_job_one.id;
        enqueue(&db, rule_job_one, NOW).await;
        let hook = manual_job(
            &seeded,
            trace("a10000000000000000000000000000b2"),
            seeded.webhook,
        );
        let hook_id = hook.id;
        enqueue(&db, hook, NOW).await;

        raw(&db, "DELETE FROM online_rules WHERE id = ?", seeded.rule).await;
        assert!(
            get_job(db.reader(), seeded.project, rule_job_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            get_job(db.reader(), seeded.project, hook_id)
                .await
                .unwrap()
                .is_some()
        );

        raw(&db, "DELETE FROM evaluators WHERE id = ?", seeded.webhook).await;
        assert!(
            get_job(db.reader(), seeded.project, hook_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_job_with_an_unknown_stored_status_is_corrupt() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let job = rule_job(&seeded, trace("a10000000000000000000000000000c1"), None);
        let id = job.id;
        enqueue(&db, job, NOW).await;
        raw(
            &db,
            "UPDATE eval_jobs SET status = 'leased' WHERE id = ?",
            id,
        )
        .await;
        let error = get_job(db.reader(), seeded.project, id).await.unwrap_err();
        assert!(
            matches!(
                error,
                MetaError::Corrupt {
                    table: "eval_jobs",
                    column: "status"
                }
            ),
            "{error}"
        );
    }
}
