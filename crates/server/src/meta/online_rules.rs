//! `online_rules` rows (spec 04, D-I-30, D-I-32). `target`, `budget_period` and
//! `paused_reason` are stored as text; a stored value outside its list is corrupt.

use llmobs_core::ids::{OrgId, ProjectId};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{MetaError, unique_or};

/// A project holds at most this many online rules.
const PROJECT_CAP: i64 = 500;

/// Pause reason: the rule's budget is spent for the period.
pub(crate) const PAUSE_BUDGET: &str = "budget";
/// Pause reason: the breaker or a webhook error pauses the rule.
pub(crate) const PAUSE_ERRORS: &str = "errors";
/// Pause reason: the judge's connection failed.
pub(crate) const PAUSE_CONNECTION: &str = "connection";

const TARGETS: [&str; 2] = ["trace", "span"];
const PERIODS: [&str; 2] = ["day", "month"];
const PAUSES: [&str; 3] = [PAUSE_BUDGET, PAUSE_ERRORS, PAUSE_CONNECTION];

/// One stored online rule.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuleRow {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// Owning project.
    pub(crate) project_id: ProjectId,
    /// The evaluator the rule runs. Immutable.
    pub(crate) evaluator_id: Uuid,
    /// Display name, unique in the project.
    pub(crate) name: String,
    /// `trace` or `span`. Immutable.
    pub(crate) target: String,
    /// Filter DSL expression, when set.
    pub(crate) filter: Option<String>,
    /// JSON object text `{variable: path}`.
    pub(crate) variable_mapping: String,
    /// Fraction of matching traces that run, in `(0, 1]`.
    pub(crate) sample_rate: f64,
    /// Budget per period, in USD.
    pub(crate) budget_usd: Option<f64>,
    /// `day` or `month`, with `budget_usd`.
    pub(crate) budget_period: Option<String>,
    /// Whether the rule is enabled.
    pub(crate) enabled: bool,
    /// Why the rule is paused, when it is.
    pub(crate) paused_reason: Option<String>,
    /// When the pause started, nanoseconds.
    pub(crate) paused_at: Option<i64>,
    /// Free text about the pause.
    pub(crate) paused_detail: Option<String>,
    /// User who created the rule.
    pub(crate) created_by: Option<Uuid>,
    /// Creation time, nanoseconds.
    pub(crate) created_at: i64,
    /// Last change time, nanoseconds.
    pub(crate) updated_at: i64,
}

/// The editable columns. Insert takes them with the id, project, evaluator, target and author;
/// update replaces them all.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuleFields {
    /// Display name, unique in the project.
    pub(crate) name: String,
    /// Filter DSL expression, when set.
    pub(crate) filter: Option<String>,
    /// JSON object text `{variable: path}`.
    pub(crate) variable_mapping: String,
    /// Fraction of matching traces that run.
    pub(crate) sample_rate: f64,
    /// Budget per period, in USD.
    pub(crate) budget_usd: Option<f64>,
    /// `day` or `month`.
    pub(crate) budget_period: Option<String>,
    /// Whether the rule is enabled.
    pub(crate) enabled: bool,
}

/// The fixed columns of a new rule.
pub(crate) struct NewRule {
    /// Chosen by the caller.
    pub(crate) id: Uuid,
    /// Owning project.
    pub(crate) project_id: ProjectId,
    /// The evaluator the rule runs.
    pub(crate) evaluator_id: Uuid,
    /// `trace` or `span`.
    pub(crate) target: String,
    /// User who creates the rule.
    pub(crate) created_by: Option<Uuid>,
}

/// Every rule of `project`, ordered by name then id.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_rules(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<Vec<RuleRow>, MetaError> {
    let records: Vec<RuleRecord> = sqlx::query_as(
        "SELECT id, project_id, evaluator_id, name, target, filter, variable_mapping, \
         sample_rate, budget_usd, budget_period, enabled, paused_reason, paused_at, \
         paused_detail, created_by, created_at, updated_at \
         FROM online_rules WHERE project_id = ? ORDER BY name, id",
    )
    .bind(project.to_string())
    .fetch_all(pool)
    .await?;
    records.into_iter().map(row_from).collect()
}

/// The rule `id` in `project`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_rule(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<RuleRow>, MetaError> {
    let record: Option<RuleRecord> = sqlx::query_as(
        "SELECT id, project_id, evaluator_id, name, target, filter, variable_mapping, \
         sample_rate, budget_usd, budget_period, enabled, paused_reason, paused_at, \
         paused_detail, created_by, created_at, updated_at \
         FROM online_rules WHERE project_id = ? AND id = ?",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    record.map(row_from).transpose()
}

/// The same read on the writer, inside a transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_rule_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<RuleRow>, MetaError> {
    let record: Option<RuleRecord> = sqlx::query_as(
        "SELECT id, project_id, evaluator_id, name, target, filter, variable_mapping, \
         sample_rate, budget_usd, budget_period, enabled, paused_reason, paused_at, \
         paused_detail, created_by, created_at, updated_at \
         FROM online_rules WHERE project_id = ? AND id = ?",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    record.map(row_from).transpose()
}

/// Inserts a rule. A project's 501st rule is [`MetaError::Conflict`] `"a project has at
/// most 500 online rules"`; a duplicate name is `"an online rule with this name already exists"`.
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails or a stored value does not decode.
pub(crate) async fn insert_rule(
    conn: &mut SqliteConnection,
    new: NewRule,
    fields: RuleFields,
    now: i64,
) -> Result<RuleRow, MetaError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM online_rules WHERE project_id = ?")
        .bind(new.project_id.to_string())
        .fetch_one(&mut *conn)
        .await?;
    if count >= PROJECT_CAP {
        return Err(MetaError::Conflict(
            "a project has at most 500 online rules",
        ));
    }
    let id = new.id;
    let project = new.project_id;
    sqlx::query(
        "INSERT INTO online_rules \
         (id, project_id, evaluator_id, name, target, filter, variable_mapping, sample_rate, \
         budget_usd, budget_period, enabled, created_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(project.to_string())
    .bind(new.evaluator_id.to_string())
    .bind(fields.name)
    .bind(new.target)
    .bind(fields.filter)
    .bind(fields.variable_mapping)
    .bind(fields.sample_rate)
    .bind(fields.budget_usd)
    .bind(fields.budget_period)
    .bind(i64::from(fields.enabled))
    .bind(new.created_by.map(|user| user.to_string()))
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "an online rule with this name already exists"))?;
    get_rule_tx(conn, project, id)
        .await?
        .ok_or_else(|| corrupt("id"))
}

/// Replaces every editable column and bumps `updated_at`. Pauses are kept. `None` means the
/// rule is missing or belongs to another project.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails or a stored value does not decode.
pub(crate) async fn update_rule(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    fields: RuleFields,
    now: i64,
) -> Result<Option<RuleRow>, MetaError> {
    let result = sqlx::query(
        "UPDATE online_rules SET name = ?, filter = ?, variable_mapping = ?, sample_rate = ?, \
         budget_usd = ?, budget_period = ?, enabled = ?, updated_at = ? \
         WHERE project_id = ? AND id = ?",
    )
    .bind(fields.name)
    .bind(fields.filter)
    .bind(fields.variable_mapping)
    .bind(fields.sample_rate)
    .bind(fields.budget_usd)
    .bind(fields.budget_period)
    .bind(i64::from(fields.enabled))
    .bind(now)
    .bind(project.to_string())
    .bind(id.to_string())
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "an online rule with this name already exists"))?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    get_rule_tx(conn, project, id)
        .await?
        .ok_or_else(|| corrupt("id"))
        .map(Some)
}

/// Deletes the rule. `false` means it is missing or belongs to another project.
///
/// # Errors
///
/// Returns [`MetaError`] when the delete fails.
pub(crate) async fn delete_rule(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM online_rules WHERE project_id = ? AND id = ?")
        .bind(project.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Enabled rules of every active project of `org`, paused ones included (D-I-32).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn count_enabled_rules(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<u64, MetaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM online_rules r \
         JOIN projects p ON p.id = r.project_id \
         WHERE p.org_id = ? AND p.state = 'active' AND r.enabled = 1",
    )
    .bind(org.to_string())
    .fetch_one(&mut *conn)
    .await?;
    u64::try_from(count).map_err(|_| corrupt("enabled"))
}

/// Every rule of `evaluator_id`, in any project, ordered by name then id.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn rules_of_evaluator(
    conn: &mut SqliteConnection,
    evaluator_id: Uuid,
) -> Result<Vec<RuleRow>, MetaError> {
    let records: Vec<RuleRecord> = sqlx::query_as(
        "SELECT id, project_id, evaluator_id, name, target, filter, variable_mapping, \
         sample_rate, budget_usd, budget_period, enabled, paused_reason, paused_at, \
         paused_detail, created_by, created_at, updated_at \
         FROM online_rules WHERE evaluator_id = ? ORDER BY name, id",
    )
    .bind(evaluator_id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    records.into_iter().map(row_from).collect()
}

/// Pauses the rule when it is enabled and not paused. `true` when a row changed.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn pause_rule(
    conn: &mut SqliteConnection,
    id: Uuid,
    reason: &'static str,
    detail: Option<&str>,
    now: i64,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "UPDATE online_rules SET paused_reason = ?, paused_at = ?, paused_detail = ?, updated_at = ? \
         WHERE id = ? AND enabled = 1 AND paused_reason IS NULL",
    )
    .bind(reason)
    .bind(now)
    .bind(detail)
    .bind(now)
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Pauses every enabled, unpaused rule whose evaluator uses `connection_id` with `'connection'`.
/// Returns the paused ids.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn pause_rules_of_connection(
    conn: &mut SqliteConnection,
    connection_id: Uuid,
    detail: Option<&str>,
    now: i64,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "UPDATE online_rules SET paused_reason = ?, paused_at = ?, paused_detail = ?, updated_at = ? \
         WHERE enabled = 1 AND paused_reason IS NULL \
         AND evaluator_id IN (SELECT id FROM evaluators WHERE connection_id = ?) \
         RETURNING id",
    )
    .bind(PAUSE_CONNECTION)
    .bind(now)
    .bind(detail)
    .bind(now)
    .bind(connection_id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    parse_ids(ids)
}

/// Pauses every enabled, unpaused rule of `evaluator_id` with `reason`. Returns the paused ids.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn pause_rules_of_evaluator(
    conn: &mut SqliteConnection,
    evaluator_id: Uuid,
    reason: &'static str,
    detail: Option<&str>,
    now: i64,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "UPDATE online_rules SET paused_reason = ?, paused_at = ?, paused_detail = ?, updated_at = ? \
         WHERE enabled = 1 AND paused_reason IS NULL AND evaluator_id = ? \
         RETURNING id",
    )
    .bind(reason)
    .bind(now)
    .bind(detail)
    .bind(now)
    .bind(evaluator_id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    parse_ids(ids)
}

/// Clears the pause of rule `id` in `project`. `None` means the rule is missing.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails or a stored value does not decode.
pub(crate) async fn resume_rule(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    now: i64,
) -> Result<Option<RuleRow>, MetaError> {
    let result = sqlx::query(
        "UPDATE online_rules SET paused_reason = NULL, paused_at = NULL, paused_detail = NULL, \
         updated_at = ? WHERE project_id = ? AND id = ?",
    )
    .bind(now)
    .bind(project.to_string())
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    get_rule_tx(conn, project, id)
        .await?
        .ok_or_else(|| corrupt("id"))
        .map(Some)
}

/// Clears the `'connection'` pauses of rules whose evaluator uses `connection_id`. Returns the ids.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn resume_connection_pauses(
    conn: &mut SqliteConnection,
    connection_id: Uuid,
    now: i64,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "UPDATE online_rules SET paused_reason = NULL, paused_at = NULL, paused_detail = NULL, \
         updated_at = ? WHERE paused_reason = ? \
         AND evaluator_id IN (SELECT id FROM evaluators WHERE connection_id = ?) \
         RETURNING id",
    )
    .bind(now)
    .bind(PAUSE_CONNECTION)
    .bind(connection_id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    parse_ids(ids)
}

/// Clears `'budget'` pauses that began before the current period: a day pause before
/// `day_start`, a month pause before `month_start`, and any budget pause of a rule that has no
/// budget any more. Returns the ids.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn clear_budget_pauses(
    conn: &mut SqliteConnection,
    day_start: i64,
    month_start: i64,
    now: i64,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "UPDATE online_rules SET paused_reason = NULL, paused_at = NULL, paused_detail = NULL, \
         updated_at = ? WHERE paused_reason = ? AND \
         ((budget_period = 'day' AND paused_at < ?) OR \
          (budget_period = 'month' AND paused_at < ?) OR budget_usd IS NULL) \
         RETURNING id",
    )
    .bind(now)
    .bind(PAUSE_BUDGET)
    .bind(day_start)
    .bind(month_start)
    .fetch_all(&mut *conn)
    .await?;
    parse_ids(ids)
}

/// The ids of every rule whose evaluator uses `connection_id`, in any project.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn rule_ids_of_connection(
    conn: &mut SqliteConnection,
    connection_id: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM online_rules \
         WHERE evaluator_id IN (SELECT id FROM evaluators WHERE connection_id = ?)",
    )
    .bind(connection_id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    parse_ids(ids)
}

/// What the trigger needs of one enabled, unpaused rule of an active project (D-I-9).
// I4.7 reads it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriggerRule {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) evaluator_id: Uuid,
    /// `"trace"` or `"span"`.
    pub(crate) target: String,
    pub(crate) filter: Option<String>,
    pub(crate) sample_rate: f64,
    /// The project's `settings.trace_completion_timeout_s`, as stored (not range-checked here).
    pub(crate) completion_timeout_s: Option<i64>,
}

/// Every enabled, unpaused rule of an active project of an active org, ordered by `project_id, id`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails, or `MetaError::Corrupt` for a stored `target`
/// outside the list or a stored id that does not parse.
// I4.5 calls it.
#[allow(dead_code)]
pub(crate) async fn trigger_rules(pool: &SqlitePool) -> Result<Vec<TriggerRule>, MetaError> {
    /// `id, project_id, evaluator_id, target, filter, sample_rate, completion_timeout_s`.
    type TriggerRecord = (
        String,
        String,
        String,
        String,
        Option<String>,
        f64,
        Option<i64>,
    );
    let records: Vec<TriggerRecord> = sqlx::query_as(
        "SELECT r.id, r.project_id, r.evaluator_id, r.target, r.filter, r.sample_rate, \
         CAST(CASE WHEN json_valid(p.settings) AND json_type(p.settings) = 'object' \
              THEN json_extract(p.settings, '$.trace_completion_timeout_s') END AS INTEGER) \
           AS completion_timeout_s \
         FROM online_rules r \
         JOIN projects p ON p.id = r.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE r.enabled = 1 AND r.paused_reason IS NULL AND p.state = 'active' AND o.state = 'active' \
         ORDER BY r.project_id, r.id",
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(
            |(id, project_id, evaluator_id, target, filter, sample_rate, completion_timeout_s)| {
                Ok(TriggerRule {
                    id: parse_uuid(&id, "id")?,
                    project_id: ProjectId::from_uuid(parse_uuid(&project_id, "project_id")?),
                    evaluator_id: parse_uuid(&evaluator_id, "evaluator_id")?,
                    target: one_of(target, &TARGETS, "target")?,
                    filter,
                    sample_rate,
                    completion_timeout_s,
                })
            },
        )
        .collect()
}

#[derive(sqlx::FromRow)]
struct RuleRecord {
    id: String,
    project_id: String,
    evaluator_id: String,
    name: String,
    target: String,
    filter: Option<String>,
    variable_mapping: String,
    sample_rate: f64,
    budget_usd: Option<f64>,
    budget_period: Option<String>,
    enabled: i64,
    paused_reason: Option<String>,
    paused_at: Option<i64>,
    paused_detail: Option<String>,
    created_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn row_from(record: RuleRecord) -> Result<RuleRow, MetaError> {
    Ok(RuleRow {
        id: parse_uuid(&record.id, "id")?,
        project_id: ProjectId::from_uuid(parse_uuid(&record.project_id, "project_id")?),
        evaluator_id: parse_uuid(&record.evaluator_id, "evaluator_id")?,
        name: record.name,
        target: one_of(record.target, &TARGETS, "target")?,
        filter: record.filter,
        variable_mapping: record.variable_mapping,
        sample_rate: record.sample_rate,
        budget_usd: record.budget_usd,
        budget_period: record
            .budget_period
            .map(|value| one_of(value, &PERIODS, "budget_period"))
            .transpose()?,
        enabled: record.enabled != 0,
        paused_reason: record
            .paused_reason
            .map(|value| one_of(value, &PAUSES, "paused_reason"))
            .transpose()?,
        paused_at: record.paused_at,
        paused_detail: record.paused_detail,
        created_by: optional_uuid(record.created_by, "created_by")?,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn one_of(value: String, allowed: &[&str], column: &'static str) -> Result<String, MetaError> {
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(corrupt(column))
    }
}

fn parse_ids(ids: Vec<String>) -> Result<Vec<Uuid>, MetaError> {
    ids.into_iter().map(|id| parse_uuid(&id, "id")).collect()
}

fn corrupt(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "online_rules",
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
    use llmobs_core::ids::{OrgId, ProjectId};
    use uuid::Uuid;

    use super::{
        NewRule, PAUSE_BUDGET, PAUSE_ERRORS, RuleFields, RuleRow, clear_budget_pauses,
        count_enabled_rules, delete_rule, get_rule, insert_rule, list_rules, pause_rule,
        pause_rules_of_connection, pause_rules_of_evaluator, resume_connection_pauses, resume_rule,
        rules_of_evaluator, update_rule,
    };
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000;

    /// An org, a project, a connection, and a judge evaluator on the connection.
    struct Seed {
        org: OrgId,
        project: ProjectId,
        connection: Uuid,
        evaluator: Uuid,
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
                let connection = Uuid::now_v7();
                insert_connection(conn, connection, org, connection_fields(), None, NOW).await?;
                let evaluator = Uuid::now_v7();
                insert_evaluator(
                    conn,
                    evaluator,
                    project,
                    "llm_judge",
                    evaluator_fields(Some(connection)),
                    None,
                    NOW,
                )
                .await?;
                Ok(Seed {
                    org,
                    project,
                    connection,
                    evaluator,
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

    fn evaluator_fields(connection: Option<Uuid>) -> EvaluatorFields {
        EvaluatorFields {
            name: "relevance".to_owned(),
            config: r#"{"model":"gpt-5-mini"}"#.to_owned(),
            output_score: r#"{"name":"relevance","data_type":"numeric"}"#.to_owned(),
            connection_id: connection,
            secret_enc: None,
            version: 1,
        }
    }

    fn fields(name: &str) -> RuleFields {
        RuleFields {
            name: name.to_owned(),
            filter: None,
            variable_mapping: r#"{"input":"trace.input"}"#.to_owned(),
            sample_rate: 1.0,
            budget_usd: None,
            budget_period: None,
            enabled: true,
        }
    }

    async fn create(db: &MetaDb, project: ProjectId, evaluator: Uuid, name: &str) -> RuleRow {
        let name = name.to_owned();
        db.write(move |conn| {
            Box::pin(async move {
                insert_rule(
                    conn,
                    NewRule {
                        id: Uuid::now_v7(),
                        project_id: project,
                        evaluator_id: evaluator,
                        target: "trace".to_owned(),
                        created_by: None,
                    },
                    fields(&name),
                    NOW,
                )
                .await
            })
        })
        .await
        .unwrap()
    }

    /// The stored pause reason, pause time and enabled flag of a rule.
    async fn rule_state(db: &MetaDb, id: Uuid) -> (Option<String>, Option<i64>, bool) {
        let (reason, at, enabled): (Option<String>, Option<i64>, i64) = sqlx::query_as(
            "SELECT paused_reason, paused_at, enabled FROM online_rules WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(db.reader())
        .await
        .unwrap();
        (reason, at, enabled != 0)
    }

    #[tokio::test]
    async fn insert_and_read_round_trip_of_every_column() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let author = db
            .write(|conn| {
                Box::pin(async move {
                    Ok(crate::meta::users::insert_user(
                        conn,
                        crate::meta::users::NewUser {
                            email: "ada@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: None,
                            is_instance_admin: false,
                        },
                        NOW,
                    )
                    .await?
                    .id)
                })
            })
            .await
            .unwrap();
        let project = seeded.project;
        let evaluator = seeded.evaluator;
        let id = Uuid::now_v7();
        let inserted = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_rule(
                        conn,
                        NewRule {
                            id,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "span".to_owned(),
                            created_by: Some(author),
                        },
                        RuleFields {
                            name: "Relevance on answers".to_owned(),
                            filter: Some("status = 'ok'".to_owned()),
                            variable_mapping: r#"{"input":"trace.input"}"#.to_owned(),
                            sample_rate: 0.25,
                            budget_usd: Some(5.0),
                            budget_period: Some("day".to_owned()),
                            enabled: true,
                        },
                        NOW,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let read = get_rule(db.reader(), project, id).await.unwrap().unwrap();
        assert_eq!(read, inserted);
        assert_eq!(read.id, id);
        assert_eq!(read.project_id, project);
        assert_eq!(read.evaluator_id, evaluator);
        assert_eq!(read.name, "Relevance on answers");
        assert_eq!(read.target, "span");
        assert_eq!(read.filter.as_deref(), Some("status = 'ok'"));
        assert_eq!(read.variable_mapping, r#"{"input":"trace.input"}"#);
        assert_eq!(read.sample_rate, 0.25);
        assert_eq!(read.budget_usd, Some(5.0));
        assert_eq!(read.budget_period.as_deref(), Some("day"));
        assert!(read.enabled);
        assert_eq!(read.paused_reason, None);
        assert_eq!(read.paused_at, None);
        assert_eq!(read.paused_detail, None);
        assert_eq!(read.created_by, Some(author));
        assert_eq!(read.created_at, NOW);
        assert_eq!(read.updated_at, NOW);
    }

    #[tokio::test]
    async fn list_is_ordered_by_name() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        create(&db, seeded.project, seeded.evaluator, "b rule").await;
        create(&db, seeded.project, seeded.evaluator, "a rule").await;
        let names: Vec<String> = list_rules(db.reader(), seeded.project)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["a rule", "b rule"]);
    }

    #[tokio::test]
    async fn duplicate_name_is_a_conflict_and_the_cap_is_500() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        create(&db, seeded.project, seeded.evaluator, "same").await;
        let (project, evaluator) = (seeded.project, seeded.evaluator);
        let duplicate = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_rule(
                        conn,
                        NewRule {
                            id: Uuid::now_v7(),
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields("same"),
                        NOW,
                    )
                    .await
                })
            })
            .await
            .unwrap_err();
        assert!(
            matches!(duplicate, MetaError::Conflict(message) if message == "an online rule with this name already exists"),
            "{duplicate}"
        );

        db.write(move |conn| {
            Box::pin(async move {
                for index in 0..499 {
                    insert_rule(
                        conn,
                        NewRule {
                            id: Uuid::now_v7(),
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields(&format!("filler {index}")),
                        NOW,
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();
        let count = list_rules(db.reader(), project).await.unwrap().len();
        assert_eq!(count, 500);
        let over = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_rule(
                        conn,
                        NewRule {
                            id: Uuid::now_v7(),
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields("the 501st"),
                        NOW,
                    )
                    .await
                })
            })
            .await
            .unwrap_err();
        assert!(
            matches!(
                over,
                MetaError::Conflict("a project has at most 500 online rules")
            ),
            "{over}"
        );
    }

    #[tokio::test]
    async fn update_of_an_unknown_id_is_none_and_delete_reports_the_row() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let rule = create(&db, seeded.project, seeded.evaluator, "one").await;
        let project = seeded.project;
        let missing = db
            .write(move |conn| {
                Box::pin(async move {
                    update_rule(conn, project, Uuid::now_v7(), fields("x"), NOW + 1).await
                })
            })
            .await
            .unwrap();
        assert!(missing.is_none());

        let id = rule.id;
        let updated = db
            .write(move |conn| {
                Box::pin(async move {
                    let mut changed = fields("renamed");
                    changed.enabled = false;
                    update_rule(conn, project, id, changed, NOW + 2).await
                })
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "renamed");
        assert!(!updated.enabled);
        assert_eq!(updated.updated_at, NOW + 2);
        assert_eq!(updated.created_at, NOW);

        let deleted = db
            .write(move |conn| Box::pin(async move { delete_rule(conn, project, id).await }))
            .await
            .unwrap();
        assert!(deleted);
        let deleted_again = db
            .write(move |conn| Box::pin(async move { delete_rule(conn, project, id).await }))
            .await
            .unwrap();
        assert!(!deleted_again);
    }

    #[tokio::test]
    async fn count_enabled_rules_spans_the_org_and_skips_deleting_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let org = seeded.org;
        let (second, other_project) = db
            .write(move |conn| {
                Box::pin(async move {
                    let second = insert_project(conn, org, "Second", "second", None, NOW)
                        .await?
                        .id;
                    let other = insert_org(conn, OrgId::new_v7(), "Other", "other", NOW)
                        .await?
                        .id;
                    let other_project =
                        insert_project(conn, other, "Elsewhere", "elsewhere", None, NOW)
                            .await?
                            .id;
                    Ok((second, other_project))
                })
            })
            .await
            .unwrap();
        // Evaluators for the second project and the other org.
        let second_evaluator = Uuid::now_v7();
        let other_evaluator = Uuid::now_v7();
        db.write(move |conn| {
            Box::pin(async move {
                insert_evaluator(
                    conn,
                    second_evaluator,
                    second,
                    "webhook",
                    evaluator_fields(None),
                    None,
                    NOW,
                )
                .await?;
                insert_evaluator(
                    conn,
                    other_evaluator,
                    other_project,
                    "webhook",
                    evaluator_fields(None),
                    None,
                    NOW,
                )
                .await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let first_project = seeded.project;
        create(&db, first_project, seeded.evaluator, "enabled").await;
        let paused = create(&db, first_project, seeded.evaluator, "paused").await;
        pause_rule_on(&db, paused.id, PAUSE_ERRORS).await;
        let disabled = create(&db, first_project, seeded.evaluator, "disabled").await;
        disable_rule(&db, disabled.id).await;
        create(&db, second, second_evaluator, "second").await;
        create(&db, other_project, other_evaluator, "elsewhere").await;

        let count = db
            .write(move |conn| Box::pin(async move { count_enabled_rules(conn, org).await }))
            .await
            .unwrap();
        assert_eq!(
            count, 3,
            "two of this org's projects, paused included, disabled excluded"
        );

        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE projects SET state = 'deleting' WHERE id = ?")
                    .bind(second.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let count = db
            .write(move |conn| Box::pin(async move { count_enabled_rules(conn, org).await }))
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    async fn pause_rule_on(db: &MetaDb, id: Uuid, reason: &'static str) {
        db.write(move |conn| {
            Box::pin(async move {
                pause_rule(conn, id, reason, Some("detail"), NOW)
                    .await
                    .map(|_| ())
            })
        })
        .await
        .unwrap();
    }

    async fn disable_rule(db: &MetaDb, id: Uuid) {
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE online_rules SET enabled = 0 WHERE id = ?")
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pause_once_and_never_a_disabled_rule() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let rule = create(&db, seeded.project, seeded.evaluator, "one").await;
        let disabled = create(&db, seeded.project, seeded.evaluator, "two").await;
        disable_rule(&db, disabled.id).await;

        let id = rule.id;
        let first = db
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, id, PAUSE_ERRORS, Some("breaker"), NOW + 5).await
                })
            })
            .await
            .unwrap();
        let second = db
            .write(move |conn| {
                Box::pin(async move { pause_rule(conn, id, PAUSE_BUDGET, None, NOW + 6).await })
            })
            .await
            .unwrap();
        assert!(first);
        assert!(!second, "a paused rule is not paused again");
        let (reason, paused_at, _) = rule_state(&db, id).await;
        assert_eq!(reason.as_deref(), Some(PAUSE_ERRORS));
        assert_eq!(paused_at, Some(NOW + 5));

        let disabled_id = disabled.id;
        let skipped = db
            .write(move |conn| {
                Box::pin(
                    async move { pause_rule(conn, disabled_id, PAUSE_ERRORS, None, NOW).await },
                )
            })
            .await
            .unwrap();
        assert!(!skipped);
        let (reason, _, _) = rule_state(&db, disabled_id).await;
        assert_eq!(reason, None);
    }

    #[tokio::test]
    async fn connection_pauses_cover_every_project_and_resume_only_their_kind() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let org = seeded.org;
        let second_project = db
            .write(move |conn| {
                Box::pin(async move {
                    Ok(insert_project(conn, org, "Second", "second", None, NOW)
                        .await?
                        .id)
                })
            })
            .await
            .unwrap();
        let second_evaluator = Uuid::now_v7();
        let connection = seeded.connection;
        db.write(move |conn| {
            Box::pin(async move {
                insert_evaluator(
                    conn,
                    second_evaluator,
                    second_project,
                    "llm_judge",
                    evaluator_fields(Some(connection)),
                    None,
                    NOW,
                )
                .await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let first = create(&db, seeded.project, seeded.evaluator, "first").await;
        let other = create(&db, second_project, second_evaluator, "other").await;
        let errored = create(&db, seeded.project, seeded.evaluator, "errored").await;
        pause_rule_on(&db, errored.id, PAUSE_ERRORS).await;

        let mut paused = db
            .write(move |conn| {
                Box::pin(async move {
                    pause_rules_of_connection(conn, connection, Some("401"), NOW + 1).await
                })
            })
            .await
            .unwrap();
        paused.sort();
        let mut expected = vec![first.id, other.id];
        expected.sort();
        assert_eq!(paused, expected);
        let (reason, _, _) = rule_state(&db, errored.id).await;
        assert_eq!(
            reason.as_deref(),
            Some(PAUSE_ERRORS),
            "an errors pause is kept"
        );

        let mut resumed = db
            .write(move |conn| {
                Box::pin(async move { resume_connection_pauses(conn, connection, NOW + 2).await })
            })
            .await
            .unwrap();
        resumed.sort();
        assert_eq!(resumed, expected);
        let (reason, _, _) = rule_state(&db, first.id).await;
        assert_eq!(reason, None);
        let (reason, _, _) = rule_state(&db, errored.id).await;
        assert_eq!(reason.as_deref(), Some(PAUSE_ERRORS));

        let by_evaluator = db
            .write(move |conn| {
                Box::pin(async move {
                    let rules = rules_of_evaluator(conn, seeded.evaluator).await?;
                    Ok(rules.len())
                })
            })
            .await
            .unwrap();
        assert_eq!(by_evaluator, 2, "the evaluator's own rules, in one project");
    }

    #[tokio::test]
    async fn budget_pauses_clear_when_their_period_has_turned() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let day_rule = create(&db, seeded.project, seeded.evaluator, "day").await;
        let month_rule = create(&db, seeded.project, seeded.evaluator, "month").await;
        let later_rule = create(&db, seeded.project, seeded.evaluator, "later").await;
        let (day_id, month_id, later_id) = (day_rule.id, month_rule.id, later_rule.id);
        db.write(move |conn| {
            Box::pin(async move {
                for (id, period) in [(day_id, "day"), (month_id, "month"), (later_id, "day")] {
                    sqlx::query(
                        "UPDATE online_rules SET budget_usd = 5, budget_period = ? WHERE id = ?",
                    )
                    .bind(period)
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                }
                pause_rule(conn, day_id, PAUSE_BUDGET, None, 100).await?;
                pause_rule(conn, month_id, PAUSE_BUDGET, None, 100).await?;
                pause_rule(conn, later_id, PAUSE_BUDGET, None, 500).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        // Day pause at 100 is before day_start 300: cleared. Month pause at 100 is not before
        // month_start 50: kept. Later day pause at 500 is not before day_start 300: kept.
        let cleared = db
            .write(move |conn| {
                Box::pin(async move { clear_budget_pauses(conn, 300, 50, NOW + 9).await })
            })
            .await
            .unwrap();
        assert_eq!(cleared, vec![day_id]);
        let (reason, _, _) = rule_state(&db, day_id).await;
        assert_eq!(reason, None);
        let (reason, _, _) = rule_state(&db, month_id).await;
        assert_eq!(reason.as_deref(), Some(PAUSE_BUDGET));
        let (reason, _, _) = rule_state(&db, later_id).await;
        assert_eq!(reason.as_deref(), Some(PAUSE_BUDGET));

        // Both remaining pauses are before the new starts: month 100 < 200 and day 500 < 600.
        let mut cleared = db
            .write(move |conn| {
                Box::pin(async move { clear_budget_pauses(conn, 600, 200, NOW + 10).await })
            })
            .await
            .unwrap();
        cleared.sort();
        let mut expected = vec![month_id, later_id];
        expected.sort();
        assert_eq!(cleared, expected);
    }

    #[tokio::test]
    async fn pause_rules_of_evaluator_pauses_only_that_evaluators_rules() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let first = create(&db, seeded.project, seeded.evaluator, "first").await;
        let second = create(&db, seeded.project, seeded.evaluator, "second").await;
        let other_evaluator = db
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        id,
                        seeded.project,
                        "webhook",
                        EvaluatorFields {
                            name: "hook".to_owned(),
                            ..evaluator_fields(None)
                        },
                        None,
                        NOW,
                    )
                    .await?;
                    Ok::<Uuid, MetaError>(id)
                })
            })
            .await
            .unwrap();
        let elsewhere = create(&db, seeded.project, other_evaluator, "elsewhere").await;

        let evaluator = seeded.evaluator;
        let mut paused = db
            .write(move |conn| {
                Box::pin(async move {
                    pause_rules_of_evaluator(
                        conn,
                        evaluator,
                        PAUSE_ERRORS,
                        Some("breaker"),
                        NOW + 4,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        paused.sort();
        let mut expected = vec![first.id, second.id];
        expected.sort();
        assert_eq!(paused, expected);
        let (reason, paused_at, _) = rule_state(&db, first.id).await;
        assert_eq!(
            (reason.as_deref(), paused_at),
            (Some(PAUSE_ERRORS), Some(NOW + 4))
        );
        let (reason, _, _) = rule_state(&db, elsewhere.id).await;
        assert_eq!(reason, None);
    }

    #[tokio::test]
    async fn an_unknown_stored_target_is_corrupt() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let rule = create(&db, seeded.project, seeded.evaluator, "one").await;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE online_rules SET target = 'session' WHERE id = ?")
                    .bind(rule.id.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let error = get_rule(db.reader(), seeded.project, rule.id)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                MetaError::Corrupt {
                    table: "online_rules",
                    column: "target"
                }
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn deleting_the_project_cascades_its_rules() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let rule = create(&db, seeded.project, seeded.evaluator, "one").await;
        let project = seeded.project;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM projects WHERE id = ?")
                    .bind(project.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM online_rules WHERE id = ?")
            .bind(rule.id.to_string())
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn resume_rule_clears_the_pause_and_reports_a_missing_rule() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let rule = create(&db, seeded.project, seeded.evaluator, "one").await;
        pause_rule_on(&db, rule.id, PAUSE_ERRORS).await;
        let project = seeded.project;
        let id = rule.id;
        let resumed = db
            .write(move |conn| {
                Box::pin(async move { resume_rule(conn, project, id, NOW + 3).await })
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.paused_reason, None);
        assert_eq!(resumed.paused_at, None);
        assert_eq!(resumed.paused_detail, None);
        let missing = db
            .write(move |conn| {
                Box::pin(async move { resume_rule(conn, project, Uuid::now_v7(), NOW).await })
            })
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    async fn set_settings(db: &MetaDb, project: ProjectId, settings: &'static str) {
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE projects SET settings = ? WHERE id = ?")
                    .bind(settings)
                    .bind(project.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    }

    async fn timeout_of_first_rule(db: &MetaDb) -> Option<i64> {
        let rules = crate::meta::online_rules::trigger_rules(db.reader())
            .await
            .unwrap();
        rules.first().unwrap().completion_timeout_s
    }

    #[tokio::test]
    async fn trigger_rules_returns_enabled_unpaused_rules_of_active_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        let enabled = create(&db, seeded.project, seeded.evaluator, "enabled").await;
        let disabled = create(&db, seeded.project, seeded.evaluator, "disabled").await;
        disable_rule(&db, disabled.id).await;
        let paused = create(&db, seeded.project, seeded.evaluator, "paused").await;
        pause_rule_on(&db, paused.id, PAUSE_BUDGET).await;
        let deleting = db
            .write(move |conn| {
                Box::pin(async move {
                    let project = insert_project(conn, seeded.org, "Other", "other", None, NOW)
                        .await?
                        .id;
                    insert_rule(
                        conn,
                        NewRule {
                            id: Uuid::now_v7(),
                            project_id: project,
                            evaluator_id: seeded.evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields("in-deleting-project"),
                        NOW,
                    )
                    .await?;
                    sqlx::query("UPDATE projects SET state = 'deleting' WHERE id = ?")
                        .bind(project.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await;
        assert!(deleting.is_ok());

        let rules = crate::meta::online_rules::trigger_rules(db.reader())
            .await
            .unwrap();
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].id, enabled.id);
        assert_eq!(rules[0].project_id, seeded.project);
        assert_eq!(rules[0].evaluator_id, seeded.evaluator);
        assert_eq!(rules[0].target, "trace");
        assert!((rules[0].sample_rate - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn trigger_rules_skips_rules_of_a_deleting_org() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        create(&db, seeded.project, seeded.evaluator, "rule").await;
        let org = seeded.org;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE orgs SET state = 'deleting' WHERE id = ?")
                    .bind(org.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let rules = crate::meta::online_rules::trigger_rules(db.reader())
            .await
            .unwrap();
        assert!(rules.is_empty());
    }

    #[tokio::test]
    async fn trigger_rules_reads_the_project_timeout() {
        let (_dir, db) = crate::meta::test_db().await;
        let seeded = seed(&db).await;
        create(&db, seeded.project, seeded.evaluator, "rule").await;
        let project = seeded.project;

        set_settings(&db, project, r#"{"trace_completion_timeout_s": 120}"#).await;
        assert_eq!(timeout_of_first_rule(&db).await, Some(120));
        set_settings(&db, project, "{}").await;
        assert_eq!(timeout_of_first_rule(&db).await, None);
        set_settings(&db, project, "not json").await;
        assert_eq!(timeout_of_first_rule(&db).await, None);
        set_settings(&db, project, r#"{"trace_completion_timeout_s": "abc"}"#).await;
        assert_eq!(timeout_of_first_rule(&db).await, Some(0));
    }
}
