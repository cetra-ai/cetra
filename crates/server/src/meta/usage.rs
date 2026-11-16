//! Daily usage read back from `usage_daily` (spec 04 § Metering, D-P-42).

use chrono::NaiveDate;
use llmobs_core::ids::{OrgId, ProjectId};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::MetaError;
use crate::api::internal::InternalUsageRow;
use crate::api::usage::UsageDay;

/// GenAI content log records of `project` received and dropped since it was created, as
/// flushed to `usage_daily` (D-G-40).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn genai_log_events(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<u64, MetaError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(genai_log_events), 0) FROM usage_daily WHERE project_id = ?",
    )
    .bind(project.to_string())
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(total).unwrap_or(0))
}

/// The org's usage per UTC day from `from` to `to`, both inclusive, summed over its projects.
/// Oldest first; days without rows are absent.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn org_days(
    pool: &SqlitePool,
    org: OrgId,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<UsageDay>, MetaError> {
    let rows: Vec<(String, i64, i64, i64, i64, i64, f64)> = sqlx::query_as(
        "SELECT day, SUM(spans), SUM(scores), SUM(bytes), SUM(judge_input_tokens), \
         SUM(judge_output_tokens), SUM(judge_cost_usd) \
         FROM usage_daily WHERE org_id = ? AND day >= ? AND day <= ? \
         GROUP BY day ORDER BY day",
    )
    .bind(org.to_string())
    .bind(from.format("%Y-%m-%d").to_string())
    .bind(to.format("%Y-%m-%d").to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(day, spans, scores, bytes, input, output, cost)| {
            let spans = u64::try_from(spans).unwrap_or(0);
            let scores = u64::try_from(scores).unwrap_or(0);
            UsageDay {
                day,
                spans,
                scores,
                units: spans.saturating_add(scores),
                bytes: u64::try_from(bytes).unwrap_or(0),
                judge_input_tokens: u64::try_from(input).unwrap_or(0),
                judge_output_tokens: u64::try_from(output).unwrap_or(0),
                judge_cost_usd: cost,
            }
        })
        .collect())
}

/// `org_id, day`, then the sums of `spans`, `scores`, `bytes`, the judge tokens and cost.
type UsageRow = (String, String, i64, i64, i64, i64, i64, f64);

/// One page of usage of every org for `from..=to` (both inclusive), ordered by `(day, org_id)`.
/// Only rows after `after` (exclusive) are returned, at most `limit`. Rows of orgs that no
/// longer exist stay: `usage_daily` has no foreign key.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored org id is not a UUID.
pub(crate) async fn all_orgs_page(
    pool: &SqlitePool,
    from: NaiveDate,
    to: NaiveDate,
    after: Option<(&str, Uuid)>,
    limit: u32,
) -> Result<Vec<InternalUsageRow>, MetaError> {
    // An empty day sorts before every day, so no cursor means "from the first row".
    let (after_day, after_org) = match after {
        Some((day, org)) => (day.to_owned(), org.to_string()),
        None => (String::new(), String::new()),
    };
    let rows: Vec<UsageRow> = sqlx::query_as(
        "SELECT org_id, day, SUM(spans), SUM(scores), SUM(bytes), SUM(judge_input_tokens), \
         SUM(judge_output_tokens), SUM(judge_cost_usd) \
         FROM usage_daily WHERE day >= ? AND day <= ? AND (day, org_id) > (?, ?) \
         GROUP BY day, org_id ORDER BY day, org_id LIMIT ?",
    )
    .bind(from.format("%Y-%m-%d").to_string())
    .bind(to.format("%Y-%m-%d").to_string())
    .bind(after_day)
    .bind(after_org)
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(org, day, spans, scores, bytes, input, output, cost)| {
            let org_id = Uuid::parse_str(&org).map_err(|_| MetaError::Corrupt {
                table: "usage_daily",
                column: "org_id",
            })?;
            let spans = u64::try_from(spans).unwrap_or(0);
            let scores = u64::try_from(scores).unwrap_or(0);
            Ok(InternalUsageRow {
                org_id,
                day,
                spans,
                scores,
                units: spans.saturating_add(scores),
                bytes: u64::try_from(bytes).unwrap_or(0),
                judge_input_tokens: u64::try_from(input).unwrap_or(0),
                judge_output_tokens: u64::try_from(output).unwrap_or(0),
                judge_cost_usd: cost,
            })
        })
        .collect()
}
