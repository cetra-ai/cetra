//! Daily usage read back from `usage_daily` (spec 04 § Metering, D-P-42).

use chrono::NaiveDate;
use llmobs_core::ids::OrgId;
use sqlx::SqlitePool;

use super::MetaError;
use crate::api::usage::UsageDay;

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
