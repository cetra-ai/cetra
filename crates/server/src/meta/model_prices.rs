//! `model_prices` rows: custom prices of an org or a project (spec 04 § Pricing, D-T-2, D-T-4).
// Used by tests only until T3 loads the rows and T4/T5 serve them; T5 removes this line.
#![cfg_attr(not(test), allow(dead_code))]

use llmobs_core::ids::{OrgId, ProjectId};
use llmobs_otel::{PriceRow, PriceScope, PriceTier};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{MetaError, unique_or};

/// An org holds at most this many org rows (D-T-4).
pub(crate) const ORG_CAP: i64 = 500;
/// A project holds at most this many rows (D-T-4).
pub(crate) const PROJECT_CAP: i64 = 500;
/// The conflict reason of the unique index.
pub(crate) const DUPLICATE: &str = "a price with this pattern, provider and valid_from exists";

/// The columns every `SELECT` reads, in [`PriceDbRecord`] order.
const COLUMNS: &str = "id, org_id, project_id, model_name, match_pattern, provider, valid_from, \
     price_input, price_output, price_cache_read, price_cache_write, price_reasoning, tiers, \
     created_at, updated_at";

/// One stored custom price.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PriceRecord {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// Owning org.
    pub(crate) org_id: OrgId,
    /// The project of a project row; `None` for an org row.
    pub(crate) project_id: Option<ProjectId>,
    /// The price. `row.scope` is `Org` or `Project`, from `project_id`.
    pub(crate) row: PriceRow,
    /// Creation time, nanoseconds.
    pub(crate) created_at: i64,
    /// Last change, nanoseconds.
    pub(crate) updated_at: i64,
}

/// A budgeted judge rule, for the write guard (D-T-8).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BudgetedRule {
    /// Rule id.
    pub(crate) id: Uuid,
    /// Rule name, for the error message.
    pub(crate) name: String,
    /// The rule's project.
    pub(crate) project_id: ProjectId,
    /// The rule's evaluator.
    pub(crate) evaluator_id: Uuid,
}

#[derive(sqlx::FromRow)]
struct PriceDbRecord {
    id: String,
    org_id: String,
    project_id: Option<String>,
    model_name: String,
    match_pattern: String,
    provider: Option<String>,
    valid_from: i64,
    price_input: Option<f64>,
    price_output: Option<f64>,
    price_cache_read: Option<f64>,
    price_cache_write: Option<f64>,
    price_reasoning: Option<f64>,
    tiers: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct BudgetedRecord {
    id: String,
    name: String,
    project_id: String,
    evaluator_id: String,
}

/// Every row, ordered by `org_id`, then `id`. Read once at startup.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_all(pool: &SqlitePool) -> Result<Vec<PriceRecord>, MetaError> {
    // The SQL is built only from constants.
    let sql = format!("SELECT {COLUMNS} FROM model_prices ORDER BY org_id, id");
    let records: Vec<PriceDbRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?;
    records.into_iter().map(record_from).collect()
}

/// Every row of `org` (org rows and the rows of all its projects), ordered by `id`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_org_all(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<Vec<PriceRecord>, MetaError> {
    // The SQL is built only from constants.
    let sql = format!("SELECT {COLUMNS} FROM model_prices WHERE org_id = ? ORDER BY id");
    let records: Vec<PriceDbRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .fetch_all(pool)
        .await?;
    records.into_iter().map(record_from).collect()
}

/// The same read inside a write transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_org_all_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<Vec<PriceRecord>, MetaError> {
    // The SQL is built only from constants.
    let sql = format!("SELECT {COLUMNS} FROM model_prices WHERE org_id = ? ORDER BY id");
    let records: Vec<PriceDbRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .fetch_all(&mut *conn)
        .await?;
    records.into_iter().map(record_from).collect()
}

/// The org rows of `org` (`project_id IS NULL`), ordered by lowercased `model_name`, then `id`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_org_rows(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<Vec<PriceRecord>, MetaError> {
    // The SQL is built only from constants.
    let sql = format!(
        "SELECT {COLUMNS} FROM model_prices WHERE org_id = ? AND project_id IS NULL \
         ORDER BY lower(model_name), id"
    );
    let records: Vec<PriceDbRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .fetch_all(pool)
        .await?;
    records.into_iter().map(record_from).collect()
}

/// The row `id` of `org` in the given scope: `project` `None` finds an org row, `Some` a row of
/// that project. A row of another scope is `None`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    id: Uuid,
) -> Result<Option<PriceRecord>, MetaError> {
    // The SQL is built only from constants.
    let sql = format!(
        "SELECT {COLUMNS} FROM model_prices WHERE org_id = ? AND id = ? AND project_id IS ?"
    );
    let record: Option<PriceDbRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .bind(id.to_string())
        .bind(project.map(|project| project.to_string()))
        .fetch_optional(&mut *conn)
        .await?;
    record.map(record_from).transpose()
}

/// Inserts a row in the scope of `project` (`None` for an org row). `id` is chosen by the caller.
///
/// The scope's cap is checked first: past it is [`MetaError::Conflict`]
/// `"an org has at most 500 org prices"` or `"a project has at most 500 prices"`. A duplicate key
/// is [`MetaError::Conflict`] [`DUPLICATE`]. `row.scope` is not read; the scope comes from `project`.
///
/// # Errors
///
/// Returns [`MetaError`] when the cap or the key rejects the row, the insert fails, or a stored
/// value does not decode.
pub(crate) async fn insert_tx(
    conn: &mut SqliteConnection,
    id: Uuid,
    org: OrgId,
    project: Option<ProjectId>,
    row: &PriceRow,
    now: i64,
) -> Result<PriceRecord, MetaError> {
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_prices WHERE org_id = ? AND project_id IS ?",
    )
    .bind(org.to_string())
    .bind(project.map(|project| project.to_string()))
    .fetch_one(&mut *conn)
    .await?;
    let (cap, reason) = match project {
        None => (ORG_CAP, "an org has at most 500 org prices"),
        Some(_) => (PROJECT_CAP, "a project has at most 500 prices"),
    };
    if live >= cap {
        return Err(MetaError::Conflict(reason));
    }
    let tiers = tiers_json(row)?;
    sqlx::query(
        "INSERT INTO model_prices \
         (id, org_id, project_id, model_name, match_pattern, provider, valid_from, price_input, \
         price_output, price_cache_read, price_cache_write, price_reasoning, tiers, created_at, \
         updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(org.to_string())
    .bind(project.map(|project| project.to_string()))
    .bind(row.model_name.clone())
    .bind(row.match_pattern.clone())
    .bind(row.provider.clone())
    .bind(row.valid_from)
    .bind(row.price_input)
    .bind(row.price_output)
    .bind(row.price_cache_read)
    .bind(row.price_cache_write)
    .bind(row.price_reasoning)
    .bind(tiers)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, DUPLICATE))?;
    get_tx(conn, org, project, id)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "model_prices",
            column: "id",
        })
}

/// Replaces every price column of the row `id` in that scope, and `updated_at`. `None` when no
/// such row. A duplicate key is [`MetaError::Conflict`] [`DUPLICATE`].
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails or a stored value does not decode.
pub(crate) async fn update_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    id: Uuid,
    row: &PriceRow,
    now: i64,
) -> Result<Option<PriceRecord>, MetaError> {
    let tiers = tiers_json(row)?;
    let result = sqlx::query(
        "UPDATE model_prices SET model_name = ?, match_pattern = ?, provider = ?, valid_from = ?, \
         price_input = ?, price_output = ?, price_cache_read = ?, price_cache_write = ?, \
         price_reasoning = ?, tiers = ?, updated_at = ? \
         WHERE org_id = ? AND id = ? AND project_id IS ?",
    )
    .bind(row.model_name.clone())
    .bind(row.match_pattern.clone())
    .bind(row.provider.clone())
    .bind(row.valid_from)
    .bind(row.price_input)
    .bind(row.price_output)
    .bind(row.price_cache_read)
    .bind(row.price_cache_write)
    .bind(row.price_reasoning)
    .bind(tiers)
    .bind(now)
    .bind(org.to_string())
    .bind(id.to_string())
    .bind(project.map(|project| project.to_string()))
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, DUPLICATE))?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    get_tx(conn, org, project, id)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "model_prices",
            column: "id",
        })
        .map(Some)
}

/// Deletes the row `id` in that scope. False when no such row.
///
/// # Errors
///
/// Returns [`MetaError`] when the delete fails.
pub(crate) async fn delete_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result =
        sqlx::query("DELETE FROM model_prices WHERE org_id = ? AND id = ? AND project_id IS ?")
            .bind(org.to_string())
            .bind(id.to_string())
            .bind(project.map(|project| project.to_string()))
            .execute(&mut *conn)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Budgeted LLM-judge rules of `org`, or of one project of it, ordered by rule id (D-T-8).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored id does not decode.
pub(crate) async fn budgeted_rules_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
) -> Result<Vec<BudgetedRule>, MetaError> {
    let project = project.map(|project| project.to_string());
    let records: Vec<BudgetedRecord> = sqlx::query_as(
        "SELECT r.id, r.name, r.project_id, r.evaluator_id \
         FROM online_rules r \
         JOIN projects p ON p.id = r.project_id \
         JOIN evaluators e ON e.id = r.evaluator_id \
         WHERE p.org_id = ? AND r.budget_usd IS NOT NULL AND e.kind = 'llm_judge' \
         AND (? IS NULL OR r.project_id = ?) \
         ORDER BY r.id",
    )
    .bind(org.to_string())
    .bind(project.clone())
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    records
        .into_iter()
        .map(|record| {
            Ok(BudgetedRule {
                id: parse_uuid(&record.id, "online_rules", "id")?,
                name: record.name,
                project_id: ProjectId::from_uuid(parse_uuid(
                    &record.project_id,
                    "online_rules",
                    "project_id",
                )?),
                evaluator_id: parse_uuid(&record.evaluator_id, "online_rules", "evaluator_id")?,
            })
        })
        .collect()
}

fn record_from(record: PriceDbRecord) -> Result<PriceRecord, MetaError> {
    let org_id = OrgId::from_uuid(parse_uuid(&record.org_id, "model_prices", "org_id")?);
    let project_id = match record.project_id.as_deref() {
        None => None,
        Some(text) => Some(ProjectId::from_uuid(parse_uuid(
            text,
            "model_prices",
            "project_id",
        )?)),
    };
    let tiers = match record.tiers {
        None => None,
        Some(text) => {
            Some(serde_json::from_str::<Vec<PriceTier>>(&text).map_err(|_| corrupt("tiers"))?)
        }
    };
    Ok(PriceRecord {
        id: parse_uuid(&record.id, "model_prices", "id")?,
        org_id,
        project_id,
        row: PriceRow {
            scope: if project_id.is_some() {
                PriceScope::Project
            } else {
                PriceScope::Org
            },
            model_name: record.model_name,
            match_pattern: record.match_pattern,
            provider: record.provider,
            valid_from: record.valid_from,
            price_input: record.price_input,
            price_output: record.price_output,
            price_cache_read: record.price_cache_read,
            price_cache_write: record.price_cache_write,
            price_reasoning: record.price_reasoning,
            tiers,
        },
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn corrupt(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "model_prices",
        column,
    }
}

fn parse_uuid(value: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    value
        .parse()
        .map_err(|_| MetaError::Corrupt { table, column })
}

/// `NULL` for no bands, so a row without tiers stores nothing.
fn tiers_json(row: &PriceRow) -> Result<Option<String>, MetaError> {
    row.tiers
        .as_ref()
        .map(|tiers| serde_json::to_string(tiers).map_err(|_| corrupt("tiers")))
        .transpose()
}

#[cfg(test)]
mod tests {
    use llmobs_core::ids::{OrgId, ProjectId};
    use llmobs_otel::{PriceRow, PriceScope, PriceTier, exact_pattern};
    use sqlx::SqliteConnection;
    use uuid::Uuid;

    use super::{
        BudgetedRule, DUPLICATE, PROJECT_CAP, PriceRecord, budgeted_rules_tx, delete_tx, get_tx,
        insert_tx, list_all, list_org_all, list_org_all_tx, list_org_rows, update_tx,
    };
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000;

    /// An org with two projects, `a` and `b`.
    async fn seed(db: &MetaDb) -> (OrgId, ProjectId, ProjectId) {
        db.write(|conn| {
            Box::pin(async move {
                let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", NOW)
                    .await?
                    .id;
                let a = insert_project(conn, org, "Support", "support", None, NOW)
                    .await?
                    .id;
                let b = insert_project(conn, org, "Sales", "sales", None, NOW)
                    .await?
                    .id;
                Ok((org, a, b))
            })
        })
        .await
        .unwrap()
    }

    /// A second org with no rows.
    async fn other_org(db: &MetaDb) -> OrgId {
        db.write(|conn| {
            Box::pin(async move {
                Ok(insert_org(conn, OrgId::new_v7(), "Other", "other", NOW)
                    .await?
                    .id)
            })
        })
        .await
        .unwrap()
    }

    /// A row that matches exactly `name`, priced at 1 and 2 per 1M tokens.
    fn price(name: &str, provider: Option<&str>) -> PriceRow {
        PriceRow {
            scope: PriceScope::Org,
            model_name: name.to_owned(),
            match_pattern: exact_pattern(name),
            provider: provider.map(str::to_owned),
            valid_from: 0,
            price_input: Some(1.0),
            price_output: Some(2.0),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        }
    }

    async fn insert(
        db: &MetaDb,
        org: OrgId,
        project: Option<ProjectId>,
        row: PriceRow,
    ) -> Result<PriceRecord, MetaError> {
        db.write(move |conn| {
            Box::pin(async move { insert_tx(conn, Uuid::now_v7(), org, project, &row, NOW).await })
        })
        .await
    }

    async fn get(
        db: &MetaDb,
        org: OrgId,
        project: Option<ProjectId>,
        id: Uuid,
    ) -> Option<PriceRecord> {
        db.write(move |conn| Box::pin(async move { get_tx(conn, org, project, id).await }))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn insert_and_get_round_trip_for_both_scopes() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, _) = seed(&db).await;
        let org_row = price("gpt-x", Some("openai"));
        let tiered = PriceRow {
            scope: PriceScope::Project,
            tiers: Some(vec![PriceTier {
                min_input_tokens: 200_000,
                price_input: Some(2.0),
                price_output: None,
                price_cache_read: None,
                price_cache_write: None,
                price_reasoning: None,
            }]),
            ..price("gpt-x", None)
        };

        let stored_org = insert(&db, org, None, org_row.clone()).await.unwrap();
        let stored_project = insert(&db, org, Some(a), tiered.clone()).await.unwrap();

        assert_eq!(stored_org.project_id, None);
        assert_eq!(stored_project.project_id, Some(a));
        for (stored, expected) in [(&stored_org, org_row), (&stored_project, tiered)] {
            let read = get(&db, org, stored.project_id, stored.id).await.unwrap();
            assert_eq!(read, *stored);
            assert_eq!(read.row, expected);
            assert_eq!(read.created_at, NOW);
            assert_eq!(read.updated_at, NOW);
        }
        assert_eq!(stored_org.row.scope, PriceScope::Org);
        assert_eq!(stored_project.row.scope, PriceScope::Project);
    }

    #[tokio::test]
    async fn get_respects_the_scope() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, b) = seed(&db).await;
        let other = other_org(&db).await;
        let project_row = insert(&db, org, Some(a), price("m", None)).await.unwrap();
        let org_row = insert(&db, org, None, price("n", None)).await.unwrap();

        assert!(get(&db, org, None, project_row.id).await.is_none());
        assert!(get(&db, org, Some(b), project_row.id).await.is_none());
        assert!(get(&db, org, Some(a), org_row.id).await.is_none());
        assert!(get(&db, other, None, org_row.id).await.is_none());
        assert!(get(&db, other, Some(a), project_row.id).await.is_none());
    }

    #[tokio::test]
    async fn a_duplicate_key_is_a_conflict() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, b) = seed(&db).await;
        insert(&db, org, None, price("m", Some("openai")))
            .await
            .unwrap();

        let error = insert(&db, org, None, price("m", Some("openai")))
            .await
            .unwrap_err();
        assert!(matches!(error, MetaError::Conflict(DUPLICATE)), "{error}");

        // The same key at another scope, or with another valid_from or provider, is allowed.
        insert(&db, org, Some(a), price("m", Some("openai")))
            .await
            .unwrap();
        insert(&db, org, Some(b), price("m", Some("openai")))
            .await
            .unwrap();
        insert(
            &db,
            org,
            None,
            PriceRow {
                valid_from: NOW,
                ..price("m", Some("openai"))
            },
        )
        .await
        .unwrap();
        insert(&db, org, None, price("m", Some("anthropic")))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_null_provider_is_part_of_the_key() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, _, _) = seed(&db).await;
        insert(&db, org, None, price("m", None)).await.unwrap();
        let error = insert(&db, org, None, price("m", None)).await.unwrap_err();
        assert!(matches!(error, MetaError::Conflict(DUPLICATE)), "{error}");
    }

    #[tokio::test]
    async fn the_caps_hold() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, b) = seed(&db).await;
        db.write(move |conn| {
            Box::pin(async move {
                for index in 0..PROJECT_CAP {
                    let row = price(&format!("m{index}"), None);
                    insert_tx(conn, Uuid::now_v7(), org, Some(a), &row, NOW).await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();

        let error = insert(&db, org, Some(a), price("one-more", None))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                MetaError::Conflict("a project has at most 500 prices")
            ),
            "{error}"
        );
        insert(&db, org, Some(b), price("one-more", None))
            .await
            .unwrap();
        insert(&db, org, None, price("one-more", None))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_replaces_every_price_column() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, _, _) = seed(&db).await;
        let row = insert(&db, org, None, price("m", Some("openai")))
            .await
            .unwrap();
        let other = insert(&db, org, None, price("n", None)).await.unwrap();

        let full = PriceRow {
            price_input: Some(3.0),
            price_output: Some(4.0),
            price_cache_read: Some(0.3),
            price_cache_write: Some(1.5),
            price_reasoning: Some(5.0),
            tiers: Some(vec![PriceTier {
                min_input_tokens: 1_000,
                price_input: Some(6.0),
                price_output: Some(7.0),
                price_cache_read: None,
                price_cache_write: None,
                price_reasoning: None,
            }]),
            ..price("m", Some("openai"))
        };
        let id = row.id;
        let updated = db
            .write({
                let full = full.clone();
                move |conn| {
                    Box::pin(async move { update_tx(conn, org, None, id, &full, NOW + 1).await })
                }
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.row, full);
        assert_eq!(updated.updated_at, NOW + 1);
        assert_eq!(updated.created_at, NOW);
        assert_eq!(get(&db, org, None, id).await.unwrap(), updated);

        let cleared = PriceRow {
            price_input: None,
            price_output: None,
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
            ..full.clone()
        };
        let again = db
            .write({
                let cleared = cleared.clone();
                move |conn| {
                    Box::pin(async move { update_tx(conn, org, None, id, &cleared, NOW + 2).await })
                }
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.row, cleared);
        assert_eq!(again.updated_at, NOW + 2);

        let missing = db
            .write(move |conn| {
                Box::pin(
                    async move { update_tx(conn, org, None, Uuid::now_v7(), &full, NOW).await },
                )
            })
            .await
            .unwrap();
        assert!(missing.is_none());

        // Moving `n` onto the key of `m` is a conflict.
        let clash = PriceRow {
            match_pattern: exact_pattern("m"),
            provider: Some("openai".to_owned()),
            ..price("n", None)
        };
        let other_id = other.id;
        let error = db
            .write(move |conn| {
                Box::pin(async move { update_tx(conn, org, None, other_id, &clash, NOW).await })
            })
            .await
            .unwrap_err();
        assert!(matches!(error, MetaError::Conflict(DUPLICATE)), "{error}");
    }

    #[tokio::test]
    async fn delete_reports_whether_a_row_went() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, b) = seed(&db).await;
        let row = insert(&db, org, Some(a), price("m", None)).await.unwrap();
        let id = row.id;

        // Another scope deletes nothing.
        let elsewhere = db
            .write(move |conn| Box::pin(async move { delete_tx(conn, org, Some(b), id).await }))
            .await
            .unwrap();
        assert!(!elsewhere);
        assert!(get(&db, org, Some(a), id).await.is_some());

        let first = db
            .write(move |conn| Box::pin(async move { delete_tx(conn, org, Some(a), id).await }))
            .await
            .unwrap();
        let second = db
            .write(move |conn| Box::pin(async move { delete_tx(conn, org, Some(a), id).await }))
            .await
            .unwrap();
        assert!(first);
        assert!(!second);
    }

    #[tokio::test]
    async fn listings_follow_their_order_and_scope() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, b) = seed(&db).await;
        insert(&db, org, None, price("b", None)).await.unwrap();
        insert(&db, org, None, price("A", None)).await.unwrap();
        insert(&db, org, None, price("c", None)).await.unwrap();
        insert(&db, org, Some(a), price("aa", None)).await.unwrap();
        insert(&db, org, Some(b), price("bb", None)).await.unwrap();

        // Org rows only, by lowercased name.
        let names: Vec<String> = list_org_rows(db.reader(), org)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.row.model_name)
            .collect();
        assert_eq!(names, ["A", "b", "c"]);

        // Every row of the org, ordered by id.
        let all = list_org_all(db.reader(), org).await.unwrap();
        assert_eq!(all.len(), 5);
        let ids: Vec<Uuid> = all.iter().map(|record| record.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        let in_tx = db
            .write(move |conn| Box::pin(async move { list_org_all_tx(conn, org).await }))
            .await
            .unwrap();
        assert_eq!(in_tx, all);
    }

    #[tokio::test]
    async fn list_all_covers_every_org() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, _) = seed(&db).await;
        let other = other_org(&db).await;
        insert(&db, org, Some(a), price("m", None)).await.unwrap();
        insert(&db, other, None, price("n", None)).await.unwrap();

        let all = list_all(db.reader()).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|record| record.org_id == org));
        assert!(all.iter().any(|record| record.org_id == other));
    }

    #[tokio::test]
    async fn rows_cascade_with_their_project_and_org() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, _) = seed(&db).await;
        insert(&db, org, None, price("org-row", None))
            .await
            .unwrap();
        insert(&db, org, Some(a), price("project-row", None))
            .await
            .unwrap();

        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM projects WHERE id = ?")
                    .bind(a.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let remaining = list_org_all(db.reader(), org).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].project_id, None);

        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM orgs WHERE id = ?")
                    .bind(org.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_prices")
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn budgeted_rules_lists_budgeted_judge_rules_only() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, a, b) = seed(&db).await;
        let other = other_org(&db).await;
        let connection = Uuid::now_v7();
        let judge_in_a = Uuid::now_v7();
        let judge_in_b = Uuid::now_v7();
        let check_in_a = Uuid::now_v7();
        let rule_a_budgeted = Uuid::now_v7();
        let rule_a_unbudgeted = Uuid::now_v7();
        let rule_a_check = Uuid::now_v7();
        let rule_b_budgeted = Uuid::now_v7();
        db.write(move |conn| {
            Box::pin(async move {
                seed_rules(
                    conn,
                    org,
                    a,
                    b,
                    connection,
                    [judge_in_a, judge_in_b, check_in_a],
                    [
                        rule_a_budgeted,
                        rule_a_unbudgeted,
                        rule_a_check,
                        rule_b_budgeted,
                    ],
                )
                .await
            })
        })
        .await
        .unwrap();

        let mut expected = vec![
            (rule_a_budgeted, a, judge_in_a),
            (rule_b_budgeted, b, judge_in_b),
        ];
        expected.sort_by_key(|(id, _, _)| *id);
        let all: Vec<(Uuid, ProjectId, Uuid)> = budgeted(&db, org, None)
            .await
            .into_iter()
            .map(|rule| (rule.id, rule.project_id, rule.evaluator_id))
            .collect();
        assert_eq!(all, expected);

        let only_a: Vec<Uuid> = budgeted(&db, org, Some(a))
            .await
            .into_iter()
            .map(|rule| rule.id)
            .collect();
        assert_eq!(only_a, [rule_a_budgeted]);

        assert!(budgeted(&db, other, None).await.is_empty());
    }

    async fn budgeted(db: &MetaDb, org: OrgId, project: Option<ProjectId>) -> Vec<BudgetedRule> {
        db.write(move |conn| Box::pin(async move { budgeted_rules_tx(conn, org, project).await }))
            .await
            .unwrap()
    }

    /// A judge in each of projects `a` and `b`, and a check in `a`, with four rules: a budgeted
    /// judge and an unbudgeted judge in `a`, a budgeted check in `a`, and a budgeted judge in `b`.
    async fn seed_rules(
        conn: &mut SqliteConnection,
        org: OrgId,
        a: ProjectId,
        b: ProjectId,
        connection: Uuid,
        evaluators: [Uuid; 3],
        rules: [Uuid; 4],
    ) -> Result<(), MetaError> {
        let [judge_in_a, judge_in_b, check_in_a] = evaluators;
        let [
            rule_a_budgeted,
            rule_a_unbudgeted,
            rule_a_check,
            rule_b_budgeted,
        ] = rules;
        insert_connection(conn, connection, org, connection_fields(), None, NOW).await?;
        insert_evaluator(
            conn,
            judge_in_a,
            a,
            "llm_judge",
            evaluator_fields("relevance", Some(connection)),
            None,
            NOW,
        )
        .await?;
        insert_evaluator(
            conn,
            judge_in_b,
            b,
            "llm_judge",
            evaluator_fields("relevance", Some(connection)),
            None,
            NOW,
        )
        .await?;
        insert_evaluator(
            conn,
            check_in_a,
            a,
            "check",
            evaluator_fields("format", None),
            None,
            NOW,
        )
        .await?;
        insert_rule(
            conn,
            NewRule {
                id: rule_a_budgeted,
                project_id: a,
                evaluator_id: judge_in_a,
                target: "trace".to_owned(),
                created_by: None,
            },
            rule_fields("judged", Some(1.0)),
            NOW,
        )
        .await?;
        insert_rule(
            conn,
            NewRule {
                id: rule_a_unbudgeted,
                project_id: a,
                evaluator_id: judge_in_a,
                target: "trace".to_owned(),
                created_by: None,
            },
            rule_fields("unbudgeted", None),
            NOW,
        )
        .await?;
        insert_rule(
            conn,
            NewRule {
                id: rule_b_budgeted,
                project_id: b,
                evaluator_id: judge_in_b,
                target: "trace".to_owned(),
                created_by: None,
            },
            rule_fields("judged", Some(1.0)),
            NOW,
        )
        .await?;
        // The route refuses a budgeted check, so the row is written directly.
        sqlx::query(
            "INSERT INTO online_rules (id, project_id, evaluator_id, name, target, \
             variable_mapping, sample_rate, budget_usd, budget_period, enabled, created_at, \
             updated_at) VALUES (?, ?, ?, 'checked', 'trace', '{}', 1.0, 1.0, 'day', 1, ?, ?)",
        )
        .bind(rule_a_check.to_string())
        .bind(a.to_string())
        .bind(check_in_a.to_string())
        .bind(NOW)
        .bind(NOW)
        .execute(&mut *conn)
        .await?;
        Ok(())
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
            output_score: format!(r#"{{"name":"{name}","data_type":"numeric"}}"#),
            connection_id: connection,
            secret_enc: None,
            version: 1,
        }
    }

    fn rule_fields(name: &str, budget_usd: Option<f64>) -> RuleFields {
        RuleFields {
            name: name.to_owned(),
            filter: None,
            variable_mapping: r#"{"input":"trace.input"}"#.to_owned(),
            sample_rate: 1.0,
            budget_usd,
            budget_period: budget_usd.map(|_| "day".to_owned()),
            enabled: true,
        }
    }
}
