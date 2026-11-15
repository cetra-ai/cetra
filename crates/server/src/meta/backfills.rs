//! `backfills` rows: an online rule's runs over stored traces (spec 04 § Evaluation; D-I-25,
//! D-I-39 to D-I-42). A backfill is `running` until the runner finishes it; startup fails the
//! ones a stop or a crash left running.

use llmobs_core::ids::ProjectId;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::eval_jobs::{self, NewJob};
use super::{MetaError, unique_or};

/// A backfill the runner has not finished.
pub(crate) const STATUS_RUNNING: &str = "running";
/// Every page was read and enqueued.
pub(crate) const STATUS_DONE: &str = "done";
/// Stopped early; `error` says why.
pub(crate) const STATUS_FAILED: &str = "failed";
/// The error of a backfill that was running when the server stopped (D-I-39).
pub(crate) const ERROR_INTERRUPTED: &str = "interrupted";
/// Most backfills `list_backfills` returns (D-I-42).
const LIST_LIMIT: i64 = 50;

const STATUSES: [&str; 3] = [STATUS_RUNNING, STATUS_DONE, STATUS_FAILED];

/// The columns `BackfillRow` reads, in the order `BackfillRecord` declares them, and the `FROM`
/// clause that joins its rule. A macro so that the queries stay string literals (sqlx takes
/// static SQL only).
macro_rules! backfill_columns {
    () => {
        "b.id, b.rule_id, r.project_id, b.from_ns, b.to_ns, b.max_targets, b.sample_rate, \
         b.status, b.matched, b.enqueued, b.error, b.created_by, b.created_at, b.finished_at \
         FROM backfills b JOIN online_rules r ON r.id = b.rule_id"
    };
}

/// One stored backfill, with its rule's project.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BackfillRow {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// The rule it runs.
    pub(crate) rule_id: Uuid,
    /// The rule's project (read through `online_rules`).
    pub(crate) project_id: ProjectId,
    /// Window start, inclusive, nanoseconds.
    pub(crate) from_ns: i64,
    /// Window end, exclusive, nanoseconds.
    pub(crate) to_ns: i64,
    /// Most targets read.
    pub(crate) max_targets: u32,
    /// Sampling rate in `(0, 1]`.
    pub(crate) sample_rate: f64,
    /// One of `STATUSES`.
    pub(crate) status: String,
    /// Targets read so far.
    pub(crate) matched: u64,
    /// Jobs inserted or re-queued so far.
    pub(crate) enqueued: u64,
    /// Why a failed backfill stopped.
    pub(crate) error: Option<String>,
    /// User who started it.
    pub(crate) created_by: Option<Uuid>,
    /// Start time, nanoseconds.
    pub(crate) created_at: i64,
    /// When it reached `done` or `failed`, nanoseconds.
    pub(crate) finished_at: Option<i64>,
}

/// A backfill to insert. `Copy`, so `insert_backfill` takes it by value without a clippy
/// `needless_pass_by_value`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NewBackfill {
    /// Chosen by the caller.
    pub(crate) id: Uuid,
    /// The rule to run.
    pub(crate) rule_id: Uuid,
    /// Window start, inclusive, nanoseconds.
    pub(crate) from_ns: i64,
    /// Window end, exclusive, nanoseconds.
    pub(crate) to_ns: i64,
    /// Most targets to read.
    pub(crate) max_targets: u32,
    /// Sampling rate in `(0, 1]`.
    pub(crate) sample_rate: f64,
    /// User who started it.
    pub(crate) created_by: Option<Uuid>,
}

/// What one page's write did (D-I-40, D-I-41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageWrite {
    /// The jobs were written; `enqueued` were inserted or re-queued.
    Written {
        /// Jobs inserted or re-queued by this page.
        enqueued: u64,
    },
    /// The rule no longer exists (its backfills went with it).
    RuleGone,
    /// The rule is disabled; nothing was written.
    RuleDisabled,
    /// The rule was edited since the run started (its `updated_at` changed); nothing was written.
    RuleChanged,
}

/// How a backfill ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackfillEnd {
    /// Every page was read and enqueued.
    Done,
    /// Stopped early, with the stored error text.
    Failed(&'static str),
}

#[derive(sqlx::FromRow)]
struct BackfillRecord {
    id: String,
    rule_id: String,
    project_id: String,
    from_ns: i64,
    to_ns: i64,
    max_targets: i64,
    sample_rate: f64,
    status: String,
    matched: i64,
    enqueued: i64,
    error: Option<String>,
    created_by: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

/// Inserts a `running` backfill and returns it.
///
/// # Errors
///
/// Returns `MetaError::Conflict` when the rule already has a running backfill, and
/// [`MetaError`] when the insert or the read-back fails.
pub(crate) async fn insert_backfill(
    conn: &mut SqliteConnection,
    new: NewBackfill,
    now: i64,
) -> Result<BackfillRow, MetaError> {
    sqlx::query(
        "INSERT INTO backfills \
         (id, rule_id, from_ns, to_ns, max_targets, sample_rate, status, matched, enqueued, \
         created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'running', 0, 0, ?, ?)",
    )
    .bind(new.id.to_string())
    .bind(new.rule_id.to_string())
    .bind(new.from_ns)
    .bind(new.to_ns)
    .bind(i64::from(new.max_targets))
    .bind(new.sample_rate)
    .bind(new.created_by.map(|id| id.to_string()))
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "a backfill of this rule is already running"))?;
    let record: BackfillRecord =
        sqlx::query_as(concat!("SELECT ", backfill_columns!(), " WHERE b.id = ?"))
            .bind(new.id.to_string())
            .fetch_one(&mut *conn)
            .await?;
    row_from(record)
}

/// The rule's newest 50 backfills, newest first (`created_at DESC, id DESC`).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_backfills(
    pool: &SqlitePool,
    rule: Uuid,
) -> Result<Vec<BackfillRow>, MetaError> {
    let records: Vec<BackfillRecord> = sqlx::query_as(concat!(
        "SELECT ",
        backfill_columns!(),
        " WHERE b.rule_id = ? ORDER BY b.created_at DESC, b.id DESC LIMIT ?"
    ))
    .bind(rule.to_string())
    .bind(LIST_LIMIT)
    .fetch_all(pool)
    .await?;
    records.into_iter().map(row_from).collect()
}

/// The `running` backfills of active projects of active orgs, oldest first.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn running_backfills(pool: &SqlitePool) -> Result<Vec<BackfillRow>, MetaError> {
    let records: Vec<BackfillRecord> = sqlx::query_as(concat!(
        "SELECT ",
        backfill_columns!(),
        " JOIN projects p ON p.id = r.project_id JOIN orgs o ON o.id = p.org_id \
         WHERE b.status = 'running' AND p.state = 'active' AND o.state = 'active' \
         ORDER BY b.created_at, b.id"
    ))
    .fetch_all(pool)
    .await?;
    records.into_iter().map(row_from).collect()
}

/// Writes one page of a backfill in one transaction (D-I-40): the rule must still exist, be
/// enabled and have the `rule_updated_at` the run started with, then every job is upserted (D-I-11)
/// and the counters are updated. `matched` is the run's total so far, not a delta; `enqueued` grows
/// by the jobs inserted or re-queued.
///
/// # Errors
///
/// Returns [`MetaError`] when a statement fails, or `MetaError::Corrupt` when a count does not fit
/// in an `i64`.
pub(crate) async fn write_page(
    conn: &mut SqliteConnection,
    backfill: Uuid,
    rule: Uuid,
    rule_updated_at: i64,
    jobs: &[NewJob],
    matched: u64,
    now: i64,
) -> Result<PageWrite, MetaError> {
    let stored: Option<(i64, i64)> =
        sqlx::query_as("SELECT enabled, updated_at FROM online_rules WHERE id = ?")
            .bind(rule.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    match stored {
        None => return Ok(PageWrite::RuleGone),
        Some((0, _)) => return Ok(PageWrite::RuleDisabled),
        Some((_, updated_at)) if updated_at != rule_updated_at => {
            return Ok(PageWrite::RuleChanged);
        }
        Some(_) => {}
    }
    let mut enqueued: u64 = 0;
    for job in jobs {
        if eval_jobs::upsert_backfill_job(conn, job, now).await? {
            enqueued += 1;
        }
    }
    sqlx::query(
        "UPDATE backfills SET matched = ?1, enqueued = enqueued + ?2 \
         WHERE id = ?3 AND status = 'running'",
    )
    .bind(count_column(matched, "matched")?)
    .bind(count_column(enqueued, "enqueued")?)
    .bind(backfill.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(PageWrite::Written { enqueued })
}

/// Ends a `running` backfill. True when a row changed; a backfill that already ended stays as it
/// is.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn finish_backfill(
    conn: &mut SqliteConnection,
    id: Uuid,
    end: BackfillEnd,
    now: i64,
) -> Result<bool, MetaError> {
    let (status, error) = match end {
        BackfillEnd::Done => (STATUS_DONE, None),
        BackfillEnd::Failed(text) => (STATUS_FAILED, Some(text)),
    };
    let result = sqlx::query(
        "UPDATE backfills SET status = ?, error = ?, finished_at = ? \
         WHERE id = ? AND status = 'running'",
    )
    .bind(status)
    .bind(error)
    .bind(now)
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Startup: marks every `running` backfill `failed` with `interrupted` (D-I-39). Returns the
/// number of rows changed.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn fail_running(conn: &mut SqliteConnection, now: i64) -> Result<u64, MetaError> {
    let result =
        sqlx::query("UPDATE backfills SET status = ?, error = ?, finished_at = ? WHERE status = ?")
            .bind(STATUS_FAILED)
            .bind(ERROR_INTERRUPTED)
            .bind(now)
            .bind(STATUS_RUNNING)
            .execute(&mut *conn)
            .await?;
    Ok(result.rows_affected())
}

fn row_from(record: BackfillRecord) -> Result<BackfillRow, MetaError> {
    Ok(BackfillRow {
        id: parse_uuid(&record.id, "id")?,
        rule_id: parse_uuid(&record.rule_id, "rule_id")?,
        project_id: ProjectId::from_uuid(parse_uuid(&record.project_id, "project_id")?),
        from_ns: record.from_ns,
        to_ns: record.to_ns,
        max_targets: u32::try_from(record.max_targets).map_err(|_| corrupt("max_targets"))?,
        sample_rate: record.sample_rate,
        status: one_of(record.status, &STATUSES, "status")?,
        matched: u64::try_from(record.matched).map_err(|_| corrupt("matched"))?,
        enqueued: u64::try_from(record.enqueued).map_err(|_| corrupt("enqueued"))?,
        error: record.error,
        created_by: record
            .created_by
            .map(|text| parse_uuid(&text, "created_by"))
            .transpose()?,
        created_at: record.created_at,
        finished_at: record.finished_at,
    })
}

fn one_of(value: String, allowed: &[&str], column: &'static str) -> Result<String, MetaError> {
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(corrupt(column))
    }
}

fn count_column(value: u64, column: &'static str) -> Result<i64, MetaError> {
    i64::try_from(value).map_err(|_| corrupt(column))
}

fn corrupt(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "backfills",
        column,
    }
}

fn parse_uuid(value: &str, column: &'static str) -> Result<Uuid, MetaError> {
    value.parse().map_err(|_| corrupt(column))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use llmobs_core::ids::{OrgId, ProjectId, TraceId};
    use sqlx::SqliteConnection;
    use uuid::Uuid;

    use super::{
        BackfillEnd, BackfillRow, NewBackfill, PageWrite, fail_running, finish_backfill,
        insert_backfill, list_backfills, running_backfills, write_page,
    };
    use crate::meta::eval_jobs::{KIND_BACKFILL, NewJob};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000_000_000_000_000_000;
    const HOUR: i64 = 3_600_000_000_000;

    struct Seed {
        org: OrgId,
        project: ProjectId,
        evaluator: Uuid,
        rule: Uuid,
    }

    async fn insert_check_rule(
        conn: &mut SqliteConnection,
        project: ProjectId,
        name: &str,
    ) -> Result<(Uuid, Uuid), MetaError> {
        let evaluator = Uuid::now_v7();
        insert_evaluator(
            conn,
            evaluator,
            project,
            "check",
            EvaluatorFields {
                name: name.to_owned(),
                config: "{}".to_owned(),
                output_score: r#"{"name":"ok","data_type":"boolean"}"#.to_owned(),
                connection_id: None,
                secret_enc: None,
                version: 1,
            },
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
                evaluator_id: evaluator,
                target: "trace".to_owned(),
                created_by: None,
            },
            RuleFields {
                name: name.to_owned(),
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
        Ok((evaluator, rule))
    }

    async fn seed(db: &MetaDb) -> Seed {
        db.write(|conn| {
            Box::pin(async move {
                let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", NOW)
                    .await?
                    .id;
                let project = insert_project(conn, org, "Support", "support", None, NOW)
                    .await?
                    .id;
                let (evaluator, rule) = insert_check_rule(conn, project, "rule").await?;
                Ok::<Seed, MetaError>(Seed {
                    org,
                    project,
                    evaluator,
                    rule,
                })
            })
        })
        .await
        .unwrap()
    }

    /// A second rule in its own project, named `slug`, and the project.
    async fn other_rule(db: &MetaDb, org: OrgId, slug: &'static str) -> (ProjectId, Uuid) {
        db.write(move |conn| {
            Box::pin(async move {
                let project = insert_project(conn, org, slug, slug, None, NOW).await?.id;
                let (_, rule) = insert_check_rule(conn, project, slug).await?;
                Ok::<(ProjectId, Uuid), MetaError>((project, rule))
            })
        })
        .await
        .unwrap()
    }

    fn new_backfill(rule: Uuid) -> NewBackfill {
        NewBackfill {
            id: Uuid::now_v7(),
            rule_id: rule,
            from_ns: NOW - HOUR,
            to_ns: NOW,
            max_targets: 100,
            sample_rate: 0.5,
            created_by: None,
        }
    }

    async fn insert(db: &MetaDb, new: NewBackfill, now: i64) -> Result<BackfillRow, MetaError> {
        db.write(move |conn| Box::pin(async move { insert_backfill(conn, new, now).await }))
            .await
    }

    async fn end(db: &MetaDb, id: Uuid, how: BackfillEnd, now: i64) -> bool {
        db.write(move |conn| Box::pin(async move { finish_backfill(conn, id, how, now).await }))
            .await
            .unwrap()
    }

    async fn list(db: &MetaDb, rule: Uuid) -> Vec<BackfillRow> {
        list_backfills(db.reader(), rule).await.unwrap()
    }

    fn trace(hex: &str) -> TraceId {
        TraceId::from_str(hex).unwrap()
    }

    fn backfill_job(seeded: &Seed, trace_id: TraceId) -> NewJob {
        NewJob {
            id: Uuid::now_v7(),
            project_id: seeded.project,
            kind: KIND_BACKFILL,
            rule_id: Some(seeded.rule),
            evaluator_id: seeded.evaluator,
            variable_mapping: None,
            trace_id,
            span_id: None,
            trace_start_ns: NOW,
        }
    }

    fn page_jobs(seeded: &Seed) -> Vec<NewJob> {
        vec![
            backfill_job(seeded, trace("0102030405060708090a0b0c0d0e0f10")),
            backfill_job(seeded, trace("0102030405060708090a0b0c0d0e0f11")),
        ]
    }

    /// A page written for a rule whose `updated_at` is `NOW`, the value `seed` inserts.
    async fn write_one_page(
        db: &MetaDb,
        backfill: Uuid,
        rule: Uuid,
        jobs: Vec<NewJob>,
        matched: u64,
    ) -> Result<PageWrite, MetaError> {
        db.write(move |conn| {
            Box::pin(
                async move { write_page(conn, backfill, rule, NOW, &jobs, matched, NOW).await },
            )
        })
        .await
    }

    async fn job_count(db: &MetaDb) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM eval_jobs")
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn insert_then_list_round_trips() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let first = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();
        assert!(end(&db, first.id, BackfillEnd::Done, NOW + 1).await);
        let second = insert(&db, new_backfill(seeded.rule), NOW + 2)
            .await
            .unwrap();

        let rows = list(&db, seeded.rule).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, second.id);
        assert_eq!(rows[0].project_id, seeded.project);
        assert_eq!(rows[0].status, "running");
        assert_eq!(rows[0].max_targets, 100);
        assert_eq!(rows[0].sample_rate, 0.5);
        assert_eq!((rows[0].matched, rows[0].enqueued), (0, 0));
        assert_eq!(rows[0].error, None);
        assert_eq!(rows[0].finished_at, None);
        assert_eq!(rows[1].id, first.id);
        assert_eq!(rows[1].status, "done");
        assert_eq!(rows[1].finished_at, Some(NOW + 1));
    }

    #[tokio::test]
    async fn a_second_running_backfill_is_a_conflict() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let first = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();
        let error = insert(&db, new_backfill(seeded.rule), NOW + 1)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            MetaError::Conflict("a backfill of this rule is already running")
        ));

        assert!(end(&db, first.id, BackfillEnd::Done, NOW + 2).await);
        insert(&db, new_backfill(seeded.rule), NOW + 3)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_list_holds_the_newest_fifty() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        for step in 0..51 {
            let now = NOW + step;
            let row = insert(&db, new_backfill(seeded.rule), now).await.unwrap();
            assert!(end(&db, row.id, BackfillEnd::Done, now).await);
        }

        let rows = list(&db, seeded.rule).await;
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0].created_at, NOW + 50);
        assert_eq!(rows[49].created_at, NOW + 1);
    }

    #[tokio::test]
    async fn running_backfills_are_oldest_first_and_of_active_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let (_, billing_rule) = other_rule(&db, seeded.org, "billing").await;
        let (old_project, old_rule) = other_rule(&db, seeded.org, "old").await;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE projects SET state = 'deleting' WHERE id = ?")
                    .bind(old_project.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();

        let newest = insert(&db, new_backfill(old_rule), NOW + 3).await.unwrap();
        let second = insert(&db, new_backfill(billing_rule), NOW + 2)
            .await
            .unwrap();
        let first = insert(&db, new_backfill(seeded.rule), NOW + 1)
            .await
            .unwrap();

        let ids: Vec<Uuid> = running_backfills(db.reader())
            .await
            .unwrap()
            .iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(ids, vec![first.id, second.id]);
        assert!(!ids.contains(&newest.id));
    }

    #[tokio::test]
    async fn write_page_writes_jobs_and_counters() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let backfill = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();
        let written = write_one_page(&db, backfill.id, seeded.rule, page_jobs(&seeded), 2)
            .await
            .unwrap();
        assert_eq!(written, PageWrite::Written { enqueued: 2 });
        let row = &list(&db, seeded.rule).await[0];
        assert_eq!((row.matched, row.enqueued), (2, 2));

        let again = write_one_page(&db, backfill.id, seeded.rule, page_jobs(&seeded), 3)
            .await
            .unwrap();
        assert_eq!(again, PageWrite::Written { enqueued: 0 });
        let row = &list(&db, seeded.rule).await[0];
        assert_eq!((row.matched, row.enqueued), (3, 2));
        assert_eq!(job_count(&db).await, 2);
    }

    #[tokio::test]
    async fn write_page_needs_an_enabled_rule() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let backfill = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();
        let rule = seeded.rule;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE online_rules SET enabled = 0 WHERE id = ?")
                    .bind(rule.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();

        let disabled = write_one_page(&db, backfill.id, seeded.rule, page_jobs(&seeded), 1)
            .await
            .unwrap();
        assert_eq!(disabled, PageWrite::RuleDisabled);
        assert_eq!(job_count(&db).await, 0);
        let row = &list(&db, seeded.rule).await[0];
        assert_eq!((row.matched, row.enqueued), (0, 0));

        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM online_rules WHERE id = ?")
                    .bind(rule.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();
        let gone = write_one_page(&db, backfill.id, seeded.rule, page_jobs(&seeded), 1)
            .await
            .unwrap();
        assert_eq!(gone, PageWrite::RuleGone);
        assert!(list(&db, seeded.rule).await.is_empty());
    }

    #[tokio::test]
    async fn write_page_refuses_an_edited_rule() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let backfill = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();
        let rule = seeded.rule;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE online_rules SET updated_at = updated_at + 1 WHERE id = ?")
                    .bind(rule.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();

        let changed = write_one_page(&db, backfill.id, seeded.rule, page_jobs(&seeded), 1)
            .await
            .unwrap();
        assert_eq!(changed, PageWrite::RuleChanged);
        assert_eq!(job_count(&db).await, 0);
        let row = &list(&db, seeded.rule).await[0];
        assert_eq!((row.matched, row.enqueued), (0, 0));
    }

    #[tokio::test]
    async fn finish_backfill_ends_a_running_backfill_once() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let backfill = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();

        let failed = BackfillEnd::Failed("rule disabled");
        assert!(end(&db, backfill.id, failed, NOW + 5).await);
        let row = &list(&db, seeded.rule).await[0];
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("rule disabled"));
        assert_eq!(row.finished_at, Some(NOW + 5));

        assert!(!end(&db, backfill.id, BackfillEnd::Done, NOW + 6).await);
        assert_eq!(list(&db, seeded.rule).await[0].status, "failed");
    }

    #[tokio::test]
    async fn fail_running_marks_interrupted() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let (_, other) = other_rule(&db, seeded.org, "other").await;
        let running = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();
        let done = insert(&db, new_backfill(other), NOW + 1).await.unwrap();
        assert!(end(&db, done.id, BackfillEnd::Done, NOW + 2).await);

        let changed: u64 = db
            .write(|conn| Box::pin(async move { fail_running(conn, NOW + 9).await }))
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let row = &list(&db, seeded.rule).await[0];
        assert_eq!(row.id, running.id);
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("interrupted"));
        assert_eq!(row.finished_at, Some(NOW + 9));
        let row = &list(&db, other).await[0];
        assert_eq!(row.status, "done");
        assert_eq!(row.error, None);
        assert_eq!(row.finished_at, Some(NOW + 2));
    }

    #[tokio::test]
    async fn a_bad_status_is_corrupt() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let backfill = insert(&db, new_backfill(seeded.rule), NOW).await.unwrap();
        let id = backfill.id;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE backfills SET status = 'paused' WHERE id = ?")
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();

        let error = list_backfills(db.reader(), seeded.rule).await.unwrap_err();
        assert!(matches!(
            error,
            MetaError::Corrupt {
                table: "backfills",
                column: "status"
            }
        ));
    }
}
