//! `rule_spend` rows: one row per rule and UTC day (D-I-15). A month is the sum of its days.
#![cfg_attr(not(test), allow(dead_code))] // I3.6 to I3.9 call these.

use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;

/// Adds `usd` to the rule's spend on `day` (UTC midnight, nanoseconds). A non-finite or negative
/// amount adds 0. A rule that no longer exists inserts nothing: a run may finish after its rule
/// was deleted, and that must not fail the finish write (§ I3.6).
///
/// # Errors
///
/// Returns [`MetaError`] when the upsert fails.
pub(crate) async fn add_spend(
    conn: &mut SqliteConnection,
    rule_id: Uuid,
    day: i64,
    usd: f64,
) -> Result<(), MetaError> {
    let usd = if usd.is_finite() && usd > 0.0 {
        usd
    } else {
        0.0
    };
    sqlx::query(
        "INSERT INTO rule_spend (rule_id, day, spent_usd) SELECT ?, ?, ? \
         WHERE EXISTS (SELECT 1 FROM online_rules WHERE id = ?) \
         ON CONFLICT(rule_id, day) DO UPDATE SET spent_usd = spent_usd + excluded.spent_usd",
    )
    .bind(rule_id.to_string())
    .bind(day)
    .bind(usd)
    .bind(rule_id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The rule's spend on the days `from_day <= day < to_day`, in USD.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn spent_between(
    pool: &SqlitePool,
    rule_id: Uuid,
    from_day: i64,
    to_day: i64,
) -> Result<f64, MetaError> {
    let total: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(spent_usd), 0.0) FROM rule_spend \
         WHERE rule_id = ? AND day >= ? AND day < ?",
    )
    .bind(rule_id.to_string())
    .bind(from_day)
    .bind(to_day)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Deletes the spend rows of every rule for days before `before_day`. Returns how many went.
///
/// # Errors
///
/// Returns [`MetaError`] when the delete fails.
pub(crate) async fn prune_spend(
    conn: &mut SqliteConnection,
    before_day: i64,
) -> Result<u64, MetaError> {
    let result = sqlx::query("DELETE FROM rule_spend WHERE day < ?")
        .bind(before_day)
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use llmobs_core::ids::ProjectId;
    use uuid::Uuid;

    use super::{add_spend, prune_spend, spent_between};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000;
    const DAY: i64 = 86_400_000_000_000;

    /// A project with one webhook evaluator and one rule. Returns the rule id.
    async fn seed_rule(db: &MetaDb) -> Uuid {
        db.write(|conn| {
            Box::pin(async move {
                let org = insert_org(conn, "Acme", "acme", NOW).await?.id;
                let project: ProjectId = insert_project(conn, org, "Support", "support", None, NOW)
                    .await?
                    .id;
                let evaluator = Uuid::now_v7();
                insert_evaluator(
                    conn,
                    evaluator,
                    project,
                    "webhook",
                    EvaluatorFields {
                        name: "hook".to_owned(),
                        config: "{}".to_owned(),
                        output_score: r#"{"name":"score","data_type":"numeric"}"#.to_owned(),
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
                        name: "rule".to_owned(),
                        filter: None,
                        variable_mapping: "{}".to_owned(),
                        sample_rate: 1.0,
                        budget_usd: Some(5.0),
                        budget_period: Some("day".to_owned()),
                        enabled: true,
                    },
                    NOW,
                )
                .await?;
                Ok::<Uuid, MetaError>(rule)
            })
        })
        .await
        .unwrap()
    }

    async fn spend(db: &MetaDb, rule: Uuid, day: i64, usd: f64) {
        db.write(move |conn| Box::pin(async move { add_spend(conn, rule, day, usd).await }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn add_spend_accumulates_per_day_and_sums_a_range() {
        let (_dir, db) = crate::meta::test_db().await;
        let rule = seed_rule(&db).await;
        let day = 10 * DAY;
        spend(&db, rule, day, 1.5).await;
        spend(&db, rule, day, 2.0).await;
        spend(&db, rule, day + DAY, 1.0).await;

        let total = spent_between(db.reader(), rule, day, day + 2 * DAY)
            .await
            .unwrap();
        assert!((total - 4.5).abs() < 1e-9, "{total}");
        let first_day = spent_between(db.reader(), rule, day, day + DAY)
            .await
            .unwrap();
        assert!((first_day - 3.5).abs() < 1e-9, "{first_day}");
        let empty = spent_between(db.reader(), rule, 0, DAY).await.unwrap();
        assert_eq!(empty, 0.0);
    }

    #[tokio::test]
    async fn a_rule_that_does_not_exist_inserts_nothing() {
        let (_dir, db) = crate::meta::test_db().await;
        let missing = Uuid::now_v7();
        spend(&db, missing, 10 * DAY, 1.0).await;
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_spend")
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn a_non_finite_or_negative_amount_adds_nothing() {
        let (_dir, db) = crate::meta::test_db().await;
        let rule = seed_rule(&db).await;
        spend(&db, rule, 10 * DAY, 2.0).await;
        spend(&db, rule, 10 * DAY, f64::NAN).await;
        spend(&db, rule, 10 * DAY, f64::INFINITY).await;
        spend(&db, rule, 10 * DAY, -3.0).await;
        let total = spent_between(db.reader(), rule, 0, 100 * DAY)
            .await
            .unwrap();
        assert!((total - 2.0).abs() < 1e-9, "{total}");
    }

    #[tokio::test]
    async fn prune_spend_deletes_the_days_before_the_cut() {
        let (_dir, db) = crate::meta::test_db().await;
        let rule = seed_rule(&db).await;
        spend(&db, rule, 10 * DAY, 1.0).await;
        spend(&db, rule, 11 * DAY, 2.0).await;
        let deleted = db
            .write(move |conn| Box::pin(async move { prune_spend(conn, 11 * DAY).await }))
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        let total = spent_between(db.reader(), rule, 0, 100 * DAY)
            .await
            .unwrap();
        assert!((total - 2.0).abs() < 1e-9, "{total}");
    }
}
