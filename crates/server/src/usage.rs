//! Usage counters (spec 04 § Metering, D-G-11, D-G-30).
//!
//! [`Usage::add`] sums accepted spans, request bytes and appended scores per org, project
//! and UTC day. [`Usage::flush`] upserts the sums into `usage_daily` and clears them.
//! `cetra serve` flushes every 10 s and once at shutdown.

use std::collections::HashMap;
use std::sync::Mutex;

use cetra_core::ids::{OrgId, ProjectId};
use chrono::{DateTime, Datelike, Months, NaiveDate};
use sqlx::SqlitePool;

use crate::meta::MetaDb;
use crate::meta::MetaError;

/// How long a dropped org's usage rows stay (D-P-46).
pub(crate) const DROPPED_USAGE_KEEP_NS: i64 = 40 * 86_400_000_000_000;

/// Deletes the `usage_daily` rows of orgs whose org drop was applied 40 or more days before
/// `now` and that have no `orgs` row. Returns the rows deleted.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn prune_dropped_orgs(meta: &MetaDb, now: i64) -> Result<u64, MetaError> {
    let applied_before = now.saturating_sub(DROPPED_USAGE_KEEP_NS);
    meta.write(move |conn| {
        Box::pin(async move {
            let result = sqlx::query(
                "DELETE FROM usage_daily \
                 WHERE org_id IN ( \
                   SELECT org_id FROM deletions \
                   WHERE kind = 'org' AND status = 'applied' AND applied_at <= ?) \
                 AND org_id NOT IN (SELECT id FROM orgs)",
            )
            .bind(applied_before)
            .execute(&mut *conn)
            .await?;
            Ok(result.rows_affected())
        })
    })
    .await
}

/// Units (spans + scores) `org_id` has flushed to `usage_daily` in the UTC month of `now_ns`.
/// Counts a running server has not flushed yet (at most 10 s) are missing: this reads only
/// stored rows (D-P-16).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn flushed_month_units(
    pool: &SqlitePool,
    org_id: OrgId,
    now_ns: i64,
) -> Result<u64, MetaError> {
    let (first, next) = month_bounds(now_ns);
    let stored: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(spans + scores), 0) FROM usage_daily \
         WHERE org_id = ? AND day >= ? AND day < ?",
    )
    .bind(org_id.to_string())
    .bind(first.format("%Y-%m-%d").to_string())
    .bind(next.format("%Y-%m-%d").to_string())
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(stored).unwrap_or(0))
}

/// One `usage_daily` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UsageKey {
    org_id: OrgId,
    project_id: ProjectId,
    day: NaiveDate,
}

/// The counts of one `usage_daily` row.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct UsageCounts {
    spans: u64,
    bytes: u64,
    scores: u64,
    judge_input_tokens: u64,
    judge_output_tokens: u64,
    judge_cost_usd: f64,
    /// GenAI content log records received on `/v1/logs` and dropped (D-G-40).
    genai_log_events: u64,
}

impl UsageCounts {
    fn merge(&mut self, other: Self) {
        self.spans = self.spans.saturating_add(other.spans);
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.scores = self.scores.saturating_add(other.scores);
        self.judge_input_tokens = self
            .judge_input_tokens
            .saturating_add(other.judge_input_tokens);
        self.judge_output_tokens = self
            .judge_output_tokens
            .saturating_add(other.judge_output_tokens);
        self.judge_cost_usd += other.judge_cost_usd;
        self.genai_log_events = self.genai_log_events.saturating_add(other.genai_log_events);
    }
}

/// Units (spans + scores) of one org in one UTC month (D-P-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonthUnits {
    /// `year * 12 + (month - 1)` of the month counted.
    month: i32,
    /// Units since 00:00 UTC on the 1st, flushed or not.
    units: u64,
}

/// In-memory usage counters, upserted into `usage_daily` (D-G-11).
#[derive(Debug, Default)]
pub(crate) struct Usage {
    counts: Mutex<HashMap<UsageKey, UsageCounts>>,
    /// Loaded month counters. Always locked while `counts` is held (lock order: counts, then months).
    months: Mutex<HashMap<OrgId, MonthUnits>>,
    /// Held by `flush` from taking the counts to the end of its write, and by
    /// `month_units` while it loads, so a load never misses counts in flight.
    flush_lock: tokio::sync::Mutex<()>,
}

/// `year * 12 + (month - 1)` of the UTC month of `now_ns`.
fn month_key(now_ns: i64) -> i32 {
    let day = DateTime::from_timestamp_nanos(now_ns).date_naive();
    day.year() * 12 + day.month0() as i32
}

/// The first day of the UTC month of `now_ns` and the first day of the next month.
#[allow(clippy::expect_used)]
pub(crate) fn month_bounds(now_ns: i64) -> (NaiveDate, NaiveDate) {
    let day = DateTime::from_timestamp_nanos(now_ns).date_naive();
    // Invariant: every month has a day 1, and the month after any date a timestamp can
    // express is still inside chrono's date range.
    let first = day.with_day(1).expect("every month has a day 1");
    let next = first
        .checked_add_months(Months::new(1))
        .expect("the month after a timestamp's month is in chrono's range");
    (first, next)
}

impl Usage {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<UsageKey, UsageCounts>> {
        self.counts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_months(&self) -> std::sync::MutexGuard<'_, HashMap<OrgId, MonthUnits>> {
        self.months
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The org's loaded units of `month`, if loaded for that month.
    /// Forgets every loaded month, so the next `month_units` loads again. Tests only.
    #[cfg(test)]
    fn forget_loaded_months(&self) {
        self.lock_months().clear();
    }

    fn loaded_units(&self, org_id: OrgId, month: i32) -> Option<u64> {
        self.lock_months()
            .get(&org_id)
            .filter(|loaded| loaded.month == month)
            .map(|loaded| loaded.units)
    }

    /// Adds counts to the day of `now_ns` (UTC). All-zero counts change nothing.
    pub(crate) fn add(
        &self,
        org_id: OrgId,
        project_id: ProjectId,
        now_ns: i64,
        spans: u64,
        bytes: u64,
        scores: u64,
    ) {
        if spans == 0 && bytes == 0 && scores == 0 {
            return;
        }
        self.merge_into(
            org_id,
            project_id,
            now_ns,
            UsageCounts {
                spans,
                bytes,
                scores,
                ..UsageCounts::default()
            },
        );
    }

    /// Adds one judge call's tokens and cost to the day of `now_ns` (UTC) (D-I-31). A zero call
    /// changes nothing. A NaN, infinite or negative cost counts as 0, so it cannot break the
    /// `judge_cost_usd` column and every later flush.
    pub(crate) fn add_judge(
        &self,
        org_id: OrgId,
        project_id: ProjectId,
        now_ns: i64,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
    ) {
        let cost_usd = if cost_usd.is_finite() && cost_usd > 0.0 {
            cost_usd
        } else {
            0.0
        };
        if input_tokens == 0 && output_tokens == 0 && cost_usd == 0.0 {
            return;
        }
        self.merge_into(
            org_id,
            project_id,
            now_ns,
            UsageCounts {
                judge_input_tokens: input_tokens,
                judge_output_tokens: output_tokens,
                judge_cost_usd: cost_usd,
                ..UsageCounts::default()
            },
        );
    }

    /// Adds GenAI content log records, received and dropped, to the day of `now_ns` (UTC)
    /// (D-G-40). Zero changes nothing.
    pub(crate) fn add_log_events(
        &self,
        org_id: OrgId,
        project_id: ProjectId,
        now_ns: i64,
        events: u64,
    ) {
        if events == 0 {
            return;
        }
        self.merge_into(
            org_id,
            project_id,
            now_ns,
            UsageCounts {
                genai_log_events: events,
                ..UsageCounts::default()
            },
        );
    }

    fn merge_into(&self, org_id: OrgId, project_id: ProjectId, now_ns: i64, counts: UsageCounts) {
        let key = UsageKey {
            org_id,
            project_id,
            day: DateTime::from_timestamp_nanos(now_ns).date_naive(),
        };
        let units = counts.spans.saturating_add(counts.scores);
        let mut rows = self.lock();
        rows.entry(key).or_default().merge(counts);
        // Still under the counts lock, so `month_units` never sees a count twice or not at all.
        let mut months = self.lock_months();
        if let Some(loaded) = months
            .get_mut(&org_id)
            .filter(|loaded| loaded.month == month_key(now_ns))
        {
            loaded.units = loaded.units.saturating_add(units);
        }
    }

    /// The org's units this UTC month at `now_ns` (D-P-16, D-P-36): `usage_daily` from the 1st
    /// plus counts not flushed yet. Loads once per org and month, then follows `add`.
    ///
    /// # Errors
    ///
    /// Returns [`MetaError`] when the load query fails.
    pub(crate) async fn month_units(
        &self,
        meta: &MetaDb,
        org_id: OrgId,
        now_ns: i64,
    ) -> Result<u64, MetaError> {
        let month = month_key(now_ns);
        if let Some(units) = self.loaded_units(org_id, month) {
            return Ok(units);
        }
        let _flush = self.flush_lock.lock().await;
        if let Some(units) = self.loaded_units(org_id, month) {
            return Ok(units);
        }
        let (first, next) = month_bounds(now_ns);
        let mut units = flushed_month_units(meta.reader(), org_id, now_ns).await?;
        {
            let counts = self.lock();
            for (key, pending) in counts.iter() {
                if key.org_id == org_id && key.day >= first && key.day < next {
                    units = units.saturating_add(pending.spans.saturating_add(pending.scores));
                }
            }
            self.lock_months()
                .insert(org_id, MonthUnits { month, units });
        }
        Ok(units)
    }

    /// Upserts the counted sums into `usage_daily` and clears them, in one transaction.
    ///
    /// The sums leave memory only once the write has committed. A write that fails, or a flush
    /// future that is dropped while it waits for the writer, puts them back for the next flush.
    ///
    /// # Errors
    ///
    /// Returns [`MetaError`] when the write fails.
    pub(crate) async fn flush(&self, meta: &MetaDb) -> Result<(), MetaError> {
        let _flush = self.flush_lock.lock().await;
        let taken = {
            let mut counts = self.lock();
            if counts.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *counts).into_iter().collect::<Vec<_>>()
        };
        let in_flight = InFlight {
            usage: self,
            rows: taken,
        };
        let rows = in_flight.rows.clone();
        meta.write(move |conn| {
            Box::pin(async move {
                for (key, counts) in rows {
                    sqlx::query(
                        "INSERT INTO usage_daily (org_id, project_id, day, spans, bytes, scores, \
                         judge_input_tokens, judge_output_tokens, judge_cost_usd, \
                         genai_log_events) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                         ON CONFLICT (org_id, project_id, day) DO UPDATE SET \
                         spans = spans + excluded.spans, \
                         bytes = bytes + excluded.bytes, \
                         scores = scores + excluded.scores, \
                         judge_input_tokens = judge_input_tokens + excluded.judge_input_tokens, \
                         judge_output_tokens = judge_output_tokens + excluded.judge_output_tokens, \
                         judge_cost_usd = judge_cost_usd + excluded.judge_cost_usd, \
                         genai_log_events = genai_log_events + excluded.genai_log_events",
                    )
                    .bind(key.org_id.to_string())
                    .bind(key.project_id.to_string())
                    .bind(key.day.format("%Y-%m-%d").to_string())
                    .bind(i64::try_from(counts.spans).unwrap_or(i64::MAX))
                    .bind(i64::try_from(counts.bytes).unwrap_or(i64::MAX))
                    .bind(i64::try_from(counts.scores).unwrap_or(i64::MAX))
                    .bind(i64::try_from(counts.judge_input_tokens).unwrap_or(i64::MAX))
                    .bind(i64::try_from(counts.judge_output_tokens).unwrap_or(i64::MAX))
                    .bind(counts.judge_cost_usd)
                    .bind(i64::try_from(counts.genai_log_events).unwrap_or(i64::MAX))
                    .execute(&mut *conn)
                    .await?;
                }
                Ok(())
            })
        })
        .await?;
        in_flight.committed();
        Ok(())
    }
}

/// Counts taken by a flush and not yet committed. Dropped without [`InFlight::committed`]
/// (the write failed, or the flush future was cancelled), it merges them back into the
/// counters, so nothing is lost. Adds made meanwhile land in the fresh map and are merged
/// with them.
struct InFlight<'a> {
    usage: &'a Usage,
    rows: Vec<(UsageKey, UsageCounts)>,
}

impl InFlight<'_> {
    /// The write committed: the taken counts are in `usage_daily` and are dropped here.
    fn committed(mut self) {
        self.rows.clear();
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let mut counts = self.usage.lock();
        for (key, taken) in self.rows.drain(..) {
            counts.entry(key).or_default().merge(taken);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::deletion::run_once;
    use crate::meta::deletions::{
        ActorKind, DeletionKind, DeletionState, NewDeletion, RequestDetails, insert_deletion,
    };
    use crate::meta::orgs::insert_org;
    use crate::meta::{MetaError, test_db};
    use crate::test_support::TestApp;

    const DAY: i64 = 86_400_000_000_000;
    /// 2026-01-01T12:00:00Z.
    const DAY_ONE: i64 = 1_767_268_800_000_000_000;
    /// 2026-01-02T12:00:00Z.
    const DAY_TWO: i64 = DAY_ONE + DAY;

    /// `(day, spans, bytes, scores)` of every `usage_daily` row of `project`, by day.
    async fn stored(meta: &MetaDb, project: ProjectId) -> Vec<(String, i64, i64, i64)> {
        sqlx::query_as(
            "SELECT day, spans, bytes, scores FROM usage_daily \
             WHERE project_id = ? ORDER BY day",
        )
        .bind(project.to_string())
        .fetch_all(meta.reader())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn flush_sums_the_counts_of_each_day() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add(org, project, DAY_ONE, 2, 100, 0);
        usage.add(org, project, DAY_ONE, 1, 50, 3);
        usage.add(org, project, DAY_TWO, 4, 7, 0);
        usage.flush(&meta).await.unwrap();
        assert_eq!(
            stored(&meta, project).await,
            vec![
                ("2026-01-01".to_owned(), 3, 150, 3),
                ("2026-01-02".to_owned(), 4, 7, 0),
            ]
        );
    }

    #[tokio::test]
    async fn log_events_are_summed_per_project() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add_log_events(org, project, DAY_ONE, 0);
        usage.add_log_events(org, project, DAY_ONE, 2);
        usage.add_log_events(org, project, DAY_TWO, 3);
        usage.flush(&meta).await.unwrap();
        usage.add_log_events(org, project, DAY_TWO, 4);
        usage.flush(&meta).await.unwrap();
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT day, genai_log_events FROM usage_daily WHERE project_id = ? ORDER BY day",
        )
        .bind(project.to_string())
        .fetch_all(meta.reader())
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![("2026-01-01".to_owned(), 2), ("2026-01-02".to_owned(), 7)]
        );
    }

    #[tokio::test]
    async fn a_second_flush_adds_to_the_stored_row() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add(org, project, DAY_ONE, 3, 0, 0);
        usage.flush(&meta).await.unwrap();
        usage.add(org, project, DAY_ONE, 2, 0, 0);
        usage.flush(&meta).await.unwrap();
        assert_eq!(
            stored(&meta, project).await,
            vec![("2026-01-01".to_owned(), 5, 0, 0)]
        );
    }

    #[tokio::test]
    async fn zero_counts_write_nothing() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add(org, project, DAY_ONE, 0, 0, 0);
        usage.flush(&meta).await.unwrap();
        assert!(stored(&meta, project).await.is_empty());
    }

    /// `(judge_input_tokens, judge_output_tokens, judge_cost_usd)` of `project`'s rows, by day.
    async fn stored_judge(meta: &MetaDb, project: ProjectId) -> Vec<(i64, i64, f64)> {
        sqlx::query_as(
            "SELECT judge_input_tokens, judge_output_tokens, judge_cost_usd FROM usage_daily \
             WHERE project_id = ? ORDER BY day",
        )
        .bind(project.to_string())
        .fetch_all(meta.reader())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn judge_usage_is_summed_and_flushed() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add_judge(org, project, DAY_ONE, 100, 20, 0.25);
        usage.add_judge(org, project, DAY_ONE, 50, 5, 0.5);
        usage.flush(&meta).await.unwrap();
        let rows = stored_judge(&meta, project).await;
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1), (150, 25));
        assert!((rows[0].2 - 0.75).abs() < 1e-12, "{rows:?}");
    }

    #[tokio::test]
    async fn zero_judge_usage_adds_nothing() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add_judge(org, project, DAY_ONE, 0, 0, 0.0);
        usage.flush(&meta).await.unwrap();
        assert!(stored(&meta, project).await.is_empty());
    }

    #[tokio::test]
    async fn judge_and_span_counts_share_a_row() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add(org, project, DAY_ONE, 2, 0, 0);
        usage.add_judge(org, project, DAY_ONE, 7, 3, 0.1);
        usage.flush(&meta).await.unwrap();
        assert_eq!(
            stored(&meta, project).await,
            vec![("2026-01-01".to_owned(), 2, 0, 0)]
        );
        let judge = stored_judge(&meta, project).await;
        assert_eq!(judge.len(), 1);
        assert_eq!(judge[0].0, 7);
    }

    #[tokio::test]
    async fn a_failed_flush_keeps_the_counts_for_the_next_one() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add(org, project, DAY_ONE, 2, 10, 1);
        meta.write(|conn| {
            Box::pin(async move {
                sqlx::query("ALTER TABLE usage_daily RENAME TO usage_held")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert!(usage.flush(&meta).await.is_err());

        meta.write(|conn| {
            Box::pin(async move {
                sqlx::query("ALTER TABLE usage_held RENAME TO usage_daily")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        usage.flush(&meta).await.unwrap();
        assert_eq!(
            stored(&meta, project).await,
            vec![("2026-01-01".to_owned(), 2, 10, 1)]
        );
    }

    #[tokio::test]
    async fn a_non_finite_judge_cost_counts_as_zero() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add_judge(org, project, DAY_ONE, 10, 2, f64::NAN);
        usage.add_judge(org, project, DAY_ONE, 1, 1, f64::INFINITY);
        usage.add_judge(org, project, DAY_ONE, 1, 1, f64::NEG_INFINITY);
        usage.add_judge(org, project, DAY_ONE, 1, 1, -0.5);
        usage.flush(&meta).await.unwrap();
        let rows = stored_judge(&meta, project).await;
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1), (13, 5));
        assert_eq!(rows[0].2, 0.0, "{rows:?}");
    }

    #[tokio::test]
    async fn a_failed_flush_keeps_judge_usage_for_the_next_one() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add(org, project, DAY_ONE, 2, 10, 1);
        usage.add_judge(org, project, DAY_ONE, 5, 7, 0.25);
        meta.write(|conn| {
            Box::pin(async move {
                sqlx::query("ALTER TABLE usage_daily RENAME TO usage_held")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert!(usage.flush(&meta).await.is_err());

        meta.write(|conn| {
            Box::pin(async move {
                sqlx::query("ALTER TABLE usage_held RENAME TO usage_daily")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        usage.flush(&meta).await.unwrap();
        assert_eq!(
            stored(&meta, project).await,
            vec![("2026-01-01".to_owned(), 2, 10, 1)]
        );
        let judge = stored_judge(&meta, project).await;
        assert_eq!(judge.len(), 1);
        assert_eq!((judge[0].0, judge[0].1), (5, 7));
        assert!((judge[0].2 - 0.25).abs() < 1e-12, "{judge:?}");
    }

    #[tokio::test]
    async fn an_aborted_flush_keeps_its_counts_for_the_next_one() {
        let (_dir, meta) = test_db().await;
        let meta = Arc::new(meta);
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Arc::new(Usage::default());
        usage.add(org, project, DAY_ONE, 2, 10, 1);
        usage.add_judge(org, project, DAY_ONE, 5, 7, 0.25);

        // Holds the single meta writer until released, so the flush waits behind it.
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let blocker = {
            let meta = Arc::clone(&meta);
            tokio::spawn(async move {
                meta.write(move |_conn| {
                    Box::pin(async move {
                        let _ = started_tx.send(());
                        let _ = release_rx.await;
                        Ok::<(), MetaError>(())
                    })
                })
                .await
            })
        };
        started_rx.await.unwrap();

        let flush = {
            let (usage, meta) = (Arc::clone(&usage), Arc::clone(&meta));
            tokio::spawn(async move { usage.flush(&meta).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!flush.is_finished(), "the flush should wait for the writer");
        // An add made while the flush is blocked must survive the abort too.
        usage.add(org, project, DAY_ONE, 1, 0, 0);
        flush.abort();
        assert!(flush.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();
        blocker.await.unwrap().unwrap();

        usage.flush(&meta).await.unwrap();
        assert_eq!(
            stored(&meta, project).await,
            vec![("2026-01-01".to_owned(), 3, 10, 1)]
        );
        let judge = stored_judge(&meta, project).await;
        assert_eq!(judge.len(), 1);
        assert_eq!((judge[0].0, judge[0].1), (5, 7));
        assert!((judge[0].2 - 0.25).abs() < 1e-12, "{judge:?}");
    }

    /// Writes `spans` and `scores` of `org` at `now_ns` into `usage_daily`, through a flush.
    async fn seed(meta: &MetaDb, org: OrgId, now_ns: i64, spans: u64, scores: u64) {
        let usage = Usage::default();
        usage.add(org, ProjectId::new_v7(), now_ns, spans, 0, scores);
        usage.flush(meta).await.unwrap();
    }

    #[tokio::test]
    async fn month_units_loads_and_follows_adds() {
        let (_dir, meta) = test_db().await;
        let (org, other, project) = (OrgId::new_v7(), OrgId::new_v7(), ProjectId::new_v7());
        seed(&meta, org, DAY_ONE, 10, 5).await;
        seed(&meta, org, DAY_TWO, 1, 0).await;
        seed(&meta, org, DAY_ONE - 17 * DAY, 100, 0).await;
        seed(&meta, other, DAY_ONE, 50, 0).await;
        let usage = Usage::default();
        assert_eq!(usage.month_units(&meta, org, DAY_ONE).await.unwrap(), 16);
        usage.add(org, project, DAY_ONE, 3, 0, 2);
        assert_eq!(usage.month_units(&meta, org, DAY_ONE).await.unwrap(), 21);
        usage.add(other, project, DAY_ONE, 9, 0, 0);
        assert_eq!(usage.month_units(&meta, org, DAY_ONE).await.unwrap(), 21);
    }

    #[tokio::test]
    async fn unflushed_counts_are_included_at_load() {
        let (_dir, meta) = test_db().await;
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let usage = Usage::default();
        usage.add(org, project, DAY_ONE, 4, 0, 0);
        assert_eq!(usage.month_units(&meta, org, DAY_ONE).await.unwrap(), 4);
        usage.flush(&meta).await.unwrap();
        assert_eq!(usage.month_units(&meta, org, DAY_ONE).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn a_new_month_starts_at_zero() {
        let (_dir, meta) = test_db().await;
        let org = OrgId::new_v7();
        // 2026-01-15T12:00:00Z.
        let jan_15 = DAY_ONE + 14 * DAY;
        // 2026-02-01T00:00:00Z.
        let feb_1 = 1_769_904_000_000_000_000;
        seed(&meta, org, jan_15, 7, 0).await;
        let usage = Usage::default();
        assert_eq!(usage.month_units(&meta, org, jan_15).await.unwrap(), 7);
        seed(&meta, org, feb_1, 3, 0).await;
        assert_eq!(usage.month_units(&meta, org, feb_1).await.unwrap(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_flush_load_and_add_lose_nothing() {
        let (_dir, meta) = test_db().await;
        let meta = Arc::new(meta);
        let usage = Arc::new(Usage::default());
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let usage = Arc::clone(&usage);
            tasks.push(tokio::task::spawn(async move {
                for _ in 0..500 {
                    usage.add(org, project, DAY_ONE, 1, 0, 0);
                }
            }));
        }
        let flusher = {
            let (usage, meta) = (Arc::clone(&usage), Arc::clone(&meta));
            tokio::task::spawn(async move {
                for _ in 0..50 {
                    usage.flush(&meta).await.unwrap();
                }
            })
        };
        let loader = {
            let (usage, meta) = (Arc::clone(&usage), Arc::clone(&meta));
            tokio::task::spawn(async move {
                for _ in 0..50 {
                    // Forget the loaded counter each round so every call really loads
                    // against the flushes and adds in flight.
                    usage.forget_loaded_months();
                    usage.month_units(&meta, org, DAY_ONE).await.unwrap();
                }
            })
        };
        for task in tasks {
            task.await.unwrap();
        }
        flusher.await.unwrap();
        loader.await.unwrap();
        assert_eq!(usage.month_units(&meta, org, DAY_ONE).await.unwrap(), 2_000);
    }

    /// An org deletion of `org`, created at `now` and still pending.
    fn org_deletion(org: OrgId, now: i64) -> NewDeletion {
        NewDeletion {
            id: uuid::Uuid::now_v7(),
            org_id: org,
            project_id: None,
            kind: DeletionKind::Org,
            user_id: None,
            request: RequestDetails::default(),
            actor_kind: ActorKind::User,
            actor_id: uuid::Uuid::now_v7(),
            state: DeletionState::Pending,
            created_at: now,
        }
    }

    /// The `usage_daily` row count of `org`.
    async fn usage_rows(meta: &MetaDb, org: OrgId) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE org_id = ?")
            .bind(org.to_string())
            .fetch_one(meta.reader())
            .await
            .unwrap()
    }

    /// Gives a new org one usage row today, then drops it through the deletion worker.
    async fn dropped_org_with_usage(app: &TestApp) -> OrgId {
        let now = app.state.clock.now_ns();
        let org = OrgId::new_v7();
        app.state.usage.add(org, ProjectId::new_v7(), now, 5, 0, 0);
        app.state.usage.flush(&app.state.meta).await.unwrap();
        let new = org_deletion(org, now);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_org(conn, org, "Dropped", "dropped", now).await?;
                    insert_deletion(conn, &new).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        run_once(&app.state).await.unwrap();
        org
    }

    #[tokio::test]
    async fn dropped_orgs_usage_is_pruned_after_40_days() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dropped = dropped_org_with_usage(&app).await;
        let applied = app.state.clock.now_ns();
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, applied, 1, 0, 0);
        app.state.usage.flush(&app.state.meta).await.unwrap();
        assert_eq!(usage_rows(&app.state.meta, dropped).await, 1);

        let early = prune_dropped_orgs(&app.state.meta, applied + 39 * DAY)
            .await
            .unwrap();
        assert_eq!(early, 0);
        assert_eq!(usage_rows(&app.state.meta, dropped).await, 1);

        let due = prune_dropped_orgs(&app.state.meta, applied + 40 * DAY)
            .await
            .unwrap();
        assert_eq!(due, 1);
        assert_eq!(usage_rows(&app.state.meta, dropped).await, 0);
        assert_eq!(usage_rows(&app.state.meta, seeded.org_id).await, 1);
    }

    #[tokio::test]
    async fn an_org_row_protects_its_usage() {
        let app = TestApp::new().await;
        let now = app.state.clock.now_ns();
        let org = OrgId::new_v7();
        app.state.usage.add(org, ProjectId::new_v7(), now, 5, 0, 0);
        app.state.usage.flush(&app.state.meta).await.unwrap();
        let new = org_deletion(org, now);
        let id = new.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_org(conn, org, "Kept", "kept", now).await?;
                    insert_deletion(conn, &new).await?;
                    sqlx::query(
                        "UPDATE deletions SET status = 'applied', applied_at = ? WHERE id = ?",
                    )
                    .bind(now)
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let pruned = prune_dropped_orgs(&app.state.meta, now + 40 * DAY)
            .await
            .unwrap();
        assert_eq!(pruned, 0);
        assert_eq!(usage_rows(&app.state.meta, org).await, 1);
    }
}
