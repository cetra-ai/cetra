//! The price tables of every org and project (spec 02 § Cost computation, D-T-3, D-T-9).
//!
//! The built-in snapshot is compiled once. An org with org rows gets a table of those rows
//! layered on the built-in one; a project with rows of its own gets a table of the org rows and
//! its rows layered on the built-in one. The ingest path reads a map under a read lock and never
//! touches `meta.db`.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use cetra_core::ids::{OrgId, ProjectId};
use cetra_eval::Adapter;
use cetra_otel::{PriceError, PriceRow, PriceScope, PriceTable, PriceTier, exact_pattern};
use sqlx::{SqliteConnection, SqlitePool};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

use crate::api::common::FieldError;
use crate::api::model_prices::{ModelPrice, ModelPriceInput, ModelPriceScope, ModelPriceTier};
use crate::audit::catalog::ModelPriceAction;
use crate::audit::{self, Event, TargetKind};
use crate::eval::budget::model_price;
use crate::eval::rule_check::Subject;
use crate::http::AppState;
use crate::meta::model_prices::{self, PriceRecord};
use crate::meta::{MetaError, connections, evaluators};

const MAX_NAME_CHARS: usize = 200;
const MAX_PATTERN_BYTES: usize = 500;
const MAX_PROVIDER_CHARS: usize = 64;
const MAX_TIERS: usize = 8;
const MAX_PRICE: f64 = 1_000_000.0;

/// Every price table of the process.
pub(crate) struct Prices {
    tables: RwLock<Tables>,
    /// Held by a price write from its transaction to its reload (README § Writes).
    writes: Mutex<()>,
}

struct Tables {
    builtin: Arc<PriceTable>,
    /// Orgs with org rows.
    orgs: HashMap<OrgId, Arc<PriceTable>>,
    /// Projects with rows of their own.
    projects: HashMap<ProjectId, Arc<PriceTable>>,
    /// The projects of each org that have an entry in `projects`, so a reload can remove them.
    org_projects: HashMap<OrgId, Vec<ProjectId>>,
}

/// The tables built from one org's rows.
pub(crate) struct OrgTables {
    /// `None` when the org has no org rows.
    pub(crate) org: Option<Arc<PriceTable>>,
    /// One entry per project with rows.
    pub(crate) projects: HashMap<ProjectId, Arc<PriceTable>>,
}

/// Builds `org`'s tables from its rows (org rows and every project's rows, `PriceRecord`s of
/// one org). The org rows come first in every project table, in the order given.
///
/// # Errors
///
/// [`PriceError`] when a table does not compile. Its index is a position in the table's rows,
/// not in `records`.
pub(crate) fn build_org(
    builtin: &Arc<PriceTable>,
    records: &[PriceRecord],
) -> Result<OrgTables, PriceError> {
    let org_rows: Vec<PriceRow> = records
        .iter()
        .filter(|record| record.project_id.is_none())
        .map(|record| record.row.clone())
        .collect();
    let org = if org_rows.is_empty() {
        None
    } else {
        Some(Arc::new(PriceTable::layered(
            Arc::clone(builtin),
            org_rows.clone(),
        )?))
    };
    let mut by_project: HashMap<ProjectId, Vec<PriceRow>> = HashMap::new();
    for record in records {
        if let Some(project) = record.project_id {
            by_project
                .entry(project)
                .or_default()
                .push(record.row.clone());
        }
    }
    let mut projects = HashMap::with_capacity(by_project.len());
    for (project, rows) in by_project {
        let mut all = org_rows.clone();
        all.extend(rows);
        projects.insert(
            project,
            Arc::new(PriceTable::layered(Arc::clone(builtin), all)?),
        );
    }
    Ok(OrgTables { org, projects })
}

impl Prices {
    /// The built-in table and no custom rows.
    pub(crate) fn new(builtin: PriceTable) -> Self {
        Self {
            tables: RwLock::new(Tables {
                builtin: Arc::new(builtin),
                orgs: HashMap::new(),
                projects: HashMap::new(),
                org_projects: HashMap::new(),
            }),
            writes: Mutex::new(()),
        }
    }

    /// The built-in table and every stored row (startup). An org whose rows do not compile
    /// prices with the built-in table; its id is logged at error level (D-T-9).
    ///
    /// # Errors
    ///
    /// [`MetaError`] when the rows cannot be read.
    pub(crate) async fn load(reader: &SqlitePool, builtin: PriceTable) -> Result<Self, MetaError> {
        let prices = Self::new(builtin);
        let records = model_prices::list_all(reader).await?;
        let mut by_org: HashMap<OrgId, Vec<PriceRecord>> = HashMap::new();
        for record in records {
            by_org.entry(record.org_id).or_default().push(record);
        }
        for (org, records) in &by_org {
            prices.install(*org, records);
        }
        Ok(prices)
    }

    /// The built-in table.
    pub(crate) fn builtin(&self) -> Arc<PriceTable> {
        Arc::clone(
            &self
                .tables
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .builtin,
        )
    }

    /// The table that prices spans of `project` in `org`: the project's, else the org's,
    /// else the built-in one.
    pub(crate) fn for_project(&self, org: OrgId, project: ProjectId) -> Arc<PriceTable> {
        let tables = self.tables.read().unwrap_or_else(PoisonError::into_inner);
        let table = tables
            .projects
            .get(&project)
            .or_else(|| tables.orgs.get(&org))
            .unwrap_or(&tables.builtin);
        Arc::clone(table)
    }

    /// The org's table, else the built-in one. For callers with no project (a connection test).
    pub(crate) fn for_org(&self, org: OrgId) -> Arc<PriceTable> {
        let tables = self.tables.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(tables.orgs.get(&org).unwrap_or(&tables.builtin))
    }

    /// Reads `org`'s rows again and swaps its tables. Rows that do not compile drop the org's
    /// tables (logged, D-T-9).
    ///
    /// # Errors
    ///
    /// [`MetaError`] when the rows cannot be read; the old tables stay.
    pub(crate) async fn reload_org(
        &self,
        reader: &SqlitePool,
        org: OrgId,
    ) -> Result<(), MetaError> {
        let records = model_prices::list_org_all(reader, org).await?;
        self.install(org, &records);
        Ok(())
    }

    /// Serializes price writes (README § Writes). Hold the guard across the write and
    /// [`Self::reload_org`].
    pub(crate) async fn lock_writes(&self) -> MutexGuard<'_, ()> {
        self.writes.lock().await
    }

    /// Drops the tables of a purged project.
    pub(crate) fn forget_project(&self, project: ProjectId) {
        let mut tables = self.tables.write().unwrap_or_else(PoisonError::into_inner);
        tables.projects.remove(&project);
        for projects in tables.org_projects.values_mut() {
            projects.retain(|id| *id != project);
        }
    }

    /// Drops the tables of a purged org and its projects.
    pub(crate) fn forget_org(&self, org: OrgId) {
        let mut tables = self.tables.write().unwrap_or_else(PoisonError::into_inner);
        tables.orgs.remove(&org);
        for project in tables.org_projects.remove(&org).unwrap_or_default() {
            tables.projects.remove(&project);
        }
    }

    /// Replaces the built-in table and rebuilds nothing else: tests only, before any custom
    /// row exists. Production never swaps the snapshot (D104).
    #[cfg(test)]
    pub(crate) fn set_builtin(&self, builtin: PriceTable) {
        let mut tables = self.tables.write().unwrap_or_else(PoisonError::into_inner);
        tables.builtin = Arc::new(builtin);
        tables.orgs.clear();
        tables.projects.clear();
        tables.org_projects.clear();
    }

    /// Replaces `org`'s tables with the ones built from `records`. A table that does not
    /// compile leaves the org on the built-in table.
    fn install(&self, org: OrgId, records: &[PriceRecord]) {
        let builtin = self.builtin();
        let built = build_org(&builtin, records);
        let mut tables = self.tables.write().unwrap_or_else(PoisonError::into_inner);
        tables.orgs.remove(&org);
        for project in tables.org_projects.remove(&org).unwrap_or_default() {
            tables.projects.remove(&project);
        }
        match built {
            Ok(built) => {
                if let Some(table) = built.org {
                    tables.orgs.insert(org, table);
                }
                let mut ids = Vec::with_capacity(built.projects.len());
                for (project, table) in built.projects {
                    tables.projects.insert(project, table);
                    ids.push(project);
                }
                tables.org_projects.insert(org, ids);
            }
            Err(_) => {
                tracing::error!(
                    org_id = %org,
                    "custom prices do not compile; the org prices with the built-in table"
                );
            }
        }
    }
}

/// One price write.
pub(crate) enum PriceChange {
    /// A new row with this id.
    Insert { id: Uuid, row: PriceRow },
    /// Replace the row `id`.
    Update { id: Uuid, row: PriceRow },
    /// Delete the row `id`.
    Delete { id: Uuid },
}

/// What a write did.
pub(crate) enum WriteOutcome {
    /// Inserted or updated: the stored row. Deleted: `None`.
    Done(Option<Box<PriceRecord>>),
    /// No row `id` in that scope.
    NotFound,
    /// The scope's table would not compile (D-T-4).
    Invalid(Vec<FieldError>),
    /// Budgeted rules would lose their price (D-T-8).
    BreaksRules(Vec<FieldError>),
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

/// Checks one price (D-T-4): finite, in `[0, 1 000 000]`.
fn check_price(errors: &mut Vec<FieldError>, field: &str, value: Option<f64>) {
    if value.is_some_and(|value| !value.is_finite() || !(0.0..=MAX_PRICE).contains(&value)) {
        errors.push(field_error(field, "must be between 0 and 1000000"));
    }
}

/// Checks a body against D-T-4 and builds the row of `scope` (`Org` or `Project`). Every
/// failing field is reported.
pub(crate) fn validate_input(
    input: &ModelPriceInput,
    scope: PriceScope,
) -> Result<PriceRow, Vec<FieldError>> {
    let mut errors = Vec::new();

    let name = input.model_name.trim();
    let name_chars = name.chars().count();
    if name.is_empty() {
        errors.push(field_error("model_name", "is required"));
    } else if name_chars > MAX_NAME_CHARS {
        errors.push(field_error("model_name", "at most 200 characters"));
    }
    let name_ok = !name.is_empty() && name_chars <= MAX_NAME_CHARS;

    let given = input
        .match_pattern
        .as_deref()
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty());
    let pattern = match given {
        None if name_ok => Some(exact_pattern(name)),
        None => None,
        Some(pattern) if pattern.len() > MAX_PATTERN_BYTES => {
            errors.push(field_error("match_pattern", "at most 500 bytes"));
            None
        }
        Some(pattern) => {
            let probe = PriceRow {
                scope,
                model_name: name.to_owned(),
                match_pattern: pattern.to_owned(),
                provider: None,
                valid_from: 0,
                price_input: None,
                price_output: None,
                price_cache_read: None,
                price_cache_write: None,
                price_reasoning: None,
                tiers: None,
            };
            match PriceTable::new(vec![probe]) {
                Ok(_) => Some(pattern.to_owned()),
                Err(PriceError::InvalidPattern { .. }) => {
                    errors.push(field_error(
                        "match_pattern",
                        "is not a valid regular expression",
                    ));
                    None
                }
                Err(PriceError::TooLarge) => {
                    errors.push(field_error("match_pattern", "is too large"));
                    None
                }
            }
        }
    };

    let provider = match input
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
    {
        None => None,
        Some(provider) => {
            let first_ok = provider
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
            let valid = first_ok
                && provider.chars().count() <= MAX_PROVIDER_CHARS
                && provider.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                });
            if valid {
                Some(provider.to_owned())
            } else {
                errors.push(field_error(
                    "provider",
                    "lowercase letters, digits, '.', '_' and '-', at most 64",
                ));
                None
            }
        }
    };

    let valid_from = match input.valid_from {
        None => 0,
        Some(value) if value < 0 => {
            errors.push(field_error("valid_from", "must not be before 1970-01-01"));
            0
        }
        Some(value) => value,
    };

    check_price(&mut errors, "price_input", input.price_input);
    check_price(&mut errors, "price_output", input.price_output);
    check_price(&mut errors, "price_cache_read", input.price_cache_read);
    check_price(&mut errors, "price_cache_write", input.price_cache_write);
    check_price(&mut errors, "price_reasoning", input.price_reasoning);
    if input.price_input.is_none() && input.price_output.is_none() {
        errors.push(field_error(
            "price_input",
            "set an input or an output price",
        ));
    }

    if input.tiers.len() > MAX_TIERS {
        errors.push(field_error("tiers", "at most 8 tiers"));
    } else {
        for (index, tier) in input.tiers.iter().enumerate() {
            let min_field = format!("tiers[{index}].min_input_tokens");
            if tier.min_input_tokens < 1 {
                errors.push(field_error(&min_field, "must be at least 1"));
            } else if index > 0 && tier.min_input_tokens <= input.tiers[index - 1].min_input_tokens
            {
                errors.push(field_error(
                    &min_field,
                    "must be greater than the previous tier's",
                ));
            }
            let part = |name: &str| format!("tiers[{index}].{name}");
            check_price(&mut errors, &part("price_input"), tier.price_input);
            check_price(&mut errors, &part("price_output"), tier.price_output);
            check_price(
                &mut errors,
                &part("price_cache_read"),
                tier.price_cache_read,
            );
            check_price(
                &mut errors,
                &part("price_cache_write"),
                tier.price_cache_write,
            );
            check_price(&mut errors, &part("price_reasoning"), tier.price_reasoning);
            if tier.price_input.is_none()
                && tier.price_output.is_none()
                && tier.price_cache_read.is_none()
                && tier.price_cache_write.is_none()
                && tier.price_reasoning.is_none()
            {
                errors.push(field_error(
                    &format!("tiers[{index}]"),
                    "set at least one price",
                ));
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    // `pattern` is `None` only after an error above, so it is `Some` here.
    let Some(match_pattern) = pattern else {
        return Err(errors);
    };
    Ok(PriceRow {
        scope,
        model_name: name.to_owned(),
        match_pattern,
        provider,
        valid_from,
        price_input: input.price_input,
        price_output: input.price_output,
        price_cache_read: input.price_cache_read,
        price_cache_write: input.price_cache_write,
        price_reasoning: input.price_reasoning,
        tiers: if input.tiers.is_empty() {
            None
        } else {
            Some(
                input
                    .tiers
                    .iter()
                    .map(|tier| PriceTier {
                        min_input_tokens: tier.min_input_tokens,
                        price_input: tier.price_input,
                        price_output: tier.price_output,
                        price_cache_read: tier.price_cache_read,
                        price_cache_write: tier.price_cache_write,
                        price_reasoning: tier.price_reasoning,
                    })
                    .collect(),
            )
        },
    })
}

/// The table of one scope: `tables` holds the org's tables (`None` when it has none); the
/// project's table if the project has one, else the org's, else the built-in one.
fn table_of<'a>(
    tables: Option<&'a OrgTables>,
    builtin: &'a Arc<PriceTable>,
    project: ProjectId,
) -> &'a PriceTable {
    let table: &Arc<PriceTable> = match tables {
        Some(t) => t
            .projects
            .get(&project)
            .or(t.org.as_ref())
            .unwrap_or(builtin),
        None => builtin,
    };
    table.as_ref()
}

/// D-T-8: the budgeted judge rules in scope that have a price before the change and none after.
async fn broken_rules(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    builtin: &Arc<PriceTable>,
    before: Option<&OrgTables>,
    after: &OrgTables,
    now: i64,
) -> Result<Vec<FieldError>, MetaError> {
    let mut errors = Vec::new();
    for rule in model_prices::budgeted_rules_tx(conn, org, project).await? {
        let Some(row) =
            evaluators::get_evaluator_tx(conn, rule.project_id, rule.evaluator_id).await?
        else {
            continue;
        };
        let subject = Subject::from_row(&row).map_err(|_| MetaError::Corrupt {
            table: "evaluators",
            column: "config",
        })?;
        // A rule without a judge is not priced at all.
        let Subject::Judge(judge) = subject else {
            continue;
        };
        // A rule whose connection is gone is broken by that, not by a price.
        let Some(connection) =
            connections::get_connection_tx(conn, org, judge.connection_id).await?
        else {
            continue;
        };
        let adapter = Adapter::from_name(&connection.adapter).map_err(|_| MetaError::Corrupt {
            table: "llm_connections",
            column: "adapter",
        })?;
        let priced_before = model_price(
            table_of(before, builtin, rule.project_id),
            &judge.model,
            adapter,
            now,
        )
        .is_some();
        let priced_after = model_price(
            table_of(Some(after), builtin, rule.project_id),
            &judge.model,
            adapter,
            now,
        )
        .is_some();
        if priced_before && !priced_after {
            errors.push(FieldError {
                field: format!("rules.{}", rule.id),
                message: format!("{}: the judge model would have no price", rule.name),
                position: None,
            });
        }
    }
    Ok(errors)
}

/// Applies `change` to the rows of `org` in the scope `project` (`None`: the org's rows), in
/// one write transaction, then reloads the org's tables (README § Writes).
///
/// # Errors
///
/// [`MetaError`] from the write: `Conflict` for the caps and the unique key.
pub(crate) async fn write(
    state: &AppState,
    org: OrgId,
    project: Option<ProjectId>,
    change: PriceChange,
    ctx: &audit::Ctx,
) -> Result<WriteOutcome, MetaError> {
    let prices = state.ingest.prices();
    let _guard = prices.lock_writes().await;
    let builtin = prices.builtin();
    let now = state.clock.now_ns();
    let closure_builtin = Arc::clone(&builtin);
    let ctx = ctx.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let records = model_prices::list_org_all_tx(conn, org).await?;
                let target = match &change {
                    PriceChange::Insert { .. } => None,
                    PriceChange::Update { id, .. } | PriceChange::Delete { id } => Some(*id),
                };
                let exists = match target {
                    Some(id) => model_prices::get_tx(conn, org, project, id)
                        .await?
                        .is_some(),
                    None => true,
                };
                if !exists {
                    return Ok(WriteOutcome::NotFound);
                }
                let mut candidate = records.clone();
                match &change {
                    PriceChange::Insert { id, row } => candidate.push(PriceRecord {
                        id: *id,
                        org_id: org,
                        project_id: project,
                        row: row.clone(),
                        created_at: now,
                        updated_at: now,
                    }),
                    PriceChange::Update { id, row } => {
                        for record in candidate.iter_mut().filter(|record| record.id == *id) {
                            record.row = row.clone();
                        }
                    }
                    PriceChange::Delete { id } => candidate.retain(|record| record.id != *id),
                }
                let after = match build_org(&closure_builtin, &candidate) {
                    Ok(tables) => tables,
                    Err(PriceError::TooLarge) => {
                        return Ok(WriteOutcome::Invalid(vec![field_error(
                            "match_pattern",
                            "the patterns of this scope are too large together",
                        )]));
                    }
                    Err(PriceError::InvalidPattern { .. }) => {
                        return Ok(WriteOutcome::Invalid(vec![field_error(
                            "match_pattern",
                            "is not a valid regular expression",
                        )]));
                    }
                };
                // Stored rows that no longer compile count as built-in only, as the registry does.
                let before = build_org(&closure_builtin, &records).ok();
                let broken = broken_rules(
                    conn,
                    org,
                    project,
                    &closure_builtin,
                    before.as_ref(),
                    &after,
                    now,
                )
                .await?;
                if !broken.is_empty() {
                    return Ok(WriteOutcome::BreaksRules(broken));
                }
                let (action, id) = match &change {
                    PriceChange::Insert { id, .. } => (ModelPriceAction::Created, *id),
                    PriceChange::Update { id, .. } => (ModelPriceAction::Updated, *id),
                    PriceChange::Delete { id } => (ModelPriceAction::Deleted, *id),
                };
                let stored = match change {
                    PriceChange::Insert { id, row } => Some(Box::new(
                        model_prices::insert_tx(conn, id, org, project, &row, now).await?,
                    )),
                    PriceChange::Update { id, row } => {
                        match model_prices::update_tx(conn, org, project, id, &row, now).await? {
                            Some(record) => Some(Box::new(record)),
                            None => return Ok(WriteOutcome::NotFound),
                        }
                    }
                    PriceChange::Delete { id } => {
                        model_prices::delete_tx(conn, org, project, id).await?;
                        None
                    }
                };
                let mut event = Event::new(action)
                    .org(org)
                    .target(TargetKind::ModelPrice, id);
                if let Some(project) = project {
                    event = event.project(project);
                }
                audit::record(conn, &ctx, event, now).await?;
                Ok(WriteOutcome::Done(stored))
            })
        })
        .await?;
    if let WriteOutcome::Done(_) = outcome {
        // The write is committed, so the answer stays a success; the next write or a restart
        // reloads the org (D-T-9).
        if let Err(error) = prices.reload_org(state.meta.reader(), org).await {
            tracing::error!(org_id = %org, error = %error, "prices were written but not reloaded");
        }
    }
    Ok(outcome)
}

/// A stored row as JSON.
pub(crate) fn record_json(record: &PriceRecord) -> ModelPrice {
    ModelPrice {
        id: Some(record.id),
        created_at: Some(record.created_at),
        updated_at: Some(record.updated_at),
        ..row_json(&record.row)
    }
}

/// A built-in row as JSON (no id, no times).
pub(crate) fn builtin_json(row: &PriceRow) -> ModelPrice {
    row_json(row)
}

fn row_json(row: &PriceRow) -> ModelPrice {
    ModelPrice {
        id: None,
        scope: match row.scope {
            PriceScope::Builtin => ModelPriceScope::Builtin,
            PriceScope::Org => ModelPriceScope::Org,
            PriceScope::Project => ModelPriceScope::Project,
        },
        model_name: row.model_name.clone(),
        match_pattern: row.match_pattern.clone(),
        provider: row.provider.clone(),
        valid_from: row.valid_from,
        price_input: row.price_input,
        price_output: row.price_output,
        price_cache_read: row.price_cache_read,
        price_cache_write: row.price_cache_write,
        price_reasoning: row.price_reasoning,
        tiers: row
            .tiers
            .iter()
            .flatten()
            .map(|tier| ModelPriceTier {
                min_input_tokens: tier.min_input_tokens,
                price_input: tier.price_input,
                price_output: tier.price_output,
                price_cache_read: tier.price_cache_read,
                price_cache_write: tier.price_cache_write,
                price_reasoning: tier.price_reasoning,
            })
            .collect(),
        created_at: None,
        updated_at: None,
    }
}

#[cfg(test)]
mod tests {
    use cetra_otel::PriceScope;
    use uuid::Uuid;

    use super::*;
    use crate::meta::model_prices::{self, PriceRecord};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;

    fn row(pattern: &str, scope: PriceScope, input: f64) -> PriceRow {
        PriceRow {
            scope,
            model_name: pattern.to_owned(),
            match_pattern: pattern.to_owned(),
            provider: None,
            valid_from: 0,
            price_input: Some(input),
            price_output: Some(1.0),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        }
    }

    fn record(org: OrgId, project: Option<ProjectId>, row: PriceRow) -> PriceRecord {
        PriceRecord {
            id: Uuid::now_v7(),
            org_id: org,
            project_id: project,
            row,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn builtin_table() -> PriceTable {
        PriceTable::new(vec![row("gpt-4o", PriceScope::Builtin, 2.5)]).expect("compiles")
    }

    fn input_price(table: &PriceTable, model: &str) -> Option<f64> {
        table
            .price_for(model, None, 1)
            .and_then(|row| row.price_input)
    }

    fn priced(prices: &Prices, org: OrgId, project: ProjectId) -> Option<f64> {
        input_price(&prices.for_project(org, project), "gpt-4o")
    }

    #[test]
    fn without_custom_rows_everything_is_builtin() {
        let prices = Prices::new(builtin_table());
        let builtin = prices.builtin();
        let (org, project) = (OrgId::new_v7(), ProjectId::new_v7());
        assert!(Arc::ptr_eq(&prices.for_project(org, project), &builtin));
        assert!(Arc::ptr_eq(&prices.for_org(org), &builtin));
    }

    #[test]
    fn build_org_layers_org_rows_under_project_rows() {
        let builtin = Arc::new(builtin_table());
        let (org, a, b) = (OrgId::new_v7(), ProjectId::new_v7(), ProjectId::new_v7());
        let records = [
            record(org, None, row("gpt-4o", PriceScope::Org, 1.0)),
            record(org, Some(a), row("gpt-4o", PriceScope::Project, 0.5)),
        ];
        let tables = build_org(&builtin, &records).expect("compiles");
        let org_table = tables.org.expect("the org has rows");
        assert_eq!(input_price(&org_table, "gpt-4o"), Some(1.0));
        let project = &tables.projects[&a];
        assert_eq!(input_price(project, "gpt-4o"), Some(0.5));
        assert_eq!(project.rows().len(), 2);
        assert_eq!(project.rows()[0].price_input, Some(1.0));
        assert_eq!(project.rows()[1].price_input, Some(0.5));
        assert!(!tables.projects.contains_key(&b));
    }

    #[test]
    fn for_project_falls_back_to_the_org_then_builtin() {
        let prices = Prices::new(builtin_table());
        let (org, a, b) = (OrgId::new_v7(), ProjectId::new_v7(), ProjectId::new_v7());
        prices.install(
            org,
            &[
                record(org, None, row("gpt-4o", PriceScope::Org, 1.0)),
                record(org, Some(a), row("gpt-4o", PriceScope::Project, 0.5)),
            ],
        );
        assert_eq!(priced(&prices, org, a), Some(0.5));
        assert_eq!(priced(&prices, org, b), Some(1.0));
        assert_eq!(
            priced(&prices, OrgId::new_v7(), ProjectId::new_v7()),
            Some(2.5)
        );
        assert_eq!(input_price(&prices.for_org(org), "gpt-4o"), Some(1.0));
    }

    #[test]
    fn an_invalid_row_leaves_the_org_on_builtin() {
        let prices = Prices::new(builtin_table());
        let (good, bad) = (OrgId::new_v7(), OrgId::new_v7());
        prices.install(
            good,
            &[record(good, None, row("gpt-4o", PriceScope::Org, 1.0))],
        );
        prices.install(bad, &[record(bad, None, row("(", PriceScope::Org, 3.0))]);
        assert!(Arc::ptr_eq(&prices.for_org(bad), &prices.builtin()));
        assert_eq!(input_price(&prices.for_org(good), "gpt-4o"), Some(1.0));
    }

    #[test]
    fn install_replaces_the_previous_tables() {
        let prices = Prices::new(builtin_table());
        let (org, a) = (OrgId::new_v7(), ProjectId::new_v7());
        prices.install(
            org,
            &[
                record(org, None, row("gpt-4o", PriceScope::Org, 1.0)),
                record(org, Some(a), row("gpt-4o", PriceScope::Project, 0.5)),
            ],
        );
        prices.install(
            org,
            &[record(
                org,
                Some(a),
                row("gpt-4o", PriceScope::Project, 0.5),
            )],
        );
        assert!(Arc::ptr_eq(&prices.for_org(org), &prices.builtin()));
        assert_eq!(priced(&prices, org, a), Some(0.5));
        prices.install(org, &[]);
        assert_eq!(priced(&prices, org, a), Some(2.5));
    }

    #[tokio::test]
    async fn load_and_reload_read_meta_db() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = OrgId::new_v7();
        let (project_a, project_b) = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_org(conn, org, "Acme", "acme", 0).await?;
                    let a = insert_project(conn, org, "A", "a", None, 0).await?;
                    let b = insert_project(conn, org, "B", "b", None, 0).await?;
                    Ok((a.id, b.id))
                })
            })
            .await
            .unwrap();
        let project_row_id = Uuid::now_v7();
        db.write(move |conn| {
            Box::pin(async move {
                model_prices::insert_tx(
                    conn,
                    Uuid::now_v7(),
                    org,
                    None,
                    &row("gpt-4o", PriceScope::Org, 1.0),
                    0,
                )
                .await?;
                model_prices::insert_tx(
                    conn,
                    project_row_id,
                    org,
                    Some(project_a),
                    &row("gpt-4o", PriceScope::Project, 0.5),
                    0,
                )
                .await
                .map(|_| ())
            })
        })
        .await
        .unwrap();

        let prices = Prices::load(db.reader(), builtin_table()).await.unwrap();
        assert_eq!(priced(&prices, org, project_a), Some(0.5));
        assert_eq!(priced(&prices, org, project_b), Some(1.0));

        db.write(move |conn| {
            Box::pin(async move {
                model_prices::delete_tx(conn, org, Some(project_a), project_row_id)
                    .await
                    .map(|_| ())
            })
        })
        .await
        .unwrap();
        prices.reload_org(db.reader(), org).await.unwrap();
        assert_eq!(priced(&prices, org, project_a), Some(1.0));
    }

    #[test]
    fn forget_drops_tables() {
        let prices = Prices::new(builtin_table());
        let (org, a) = (OrgId::new_v7(), ProjectId::new_v7());
        prices.install(
            org,
            &[
                record(org, None, row("gpt-4o", PriceScope::Org, 1.0)),
                record(org, Some(a), row("gpt-4o", PriceScope::Project, 0.5)),
            ],
        );
        prices.forget_project(a);
        assert!(Arc::ptr_eq(
            &prices.for_project(org, a),
            &prices.for_org(org)
        ));
        prices.forget_org(org);
        assert!(Arc::ptr_eq(&prices.for_project(org, a), &prices.builtin()));
        assert!(Arc::ptr_eq(&prices.for_org(org), &prices.builtin()));
    }

    #[test]
    fn for_project_is_cheap_to_share() {
        let prices = Prices::new(builtin_table());
        let (org, a) = (OrgId::new_v7(), ProjectId::new_v7());
        prices.install(
            org,
            &[record(
                org,
                Some(a),
                row("gpt-4o", PriceScope::Project, 0.5),
            )],
        );
        assert!(Arc::ptr_eq(
            &prices.for_project(org, a),
            &prices.for_project(org, a)
        ));
    }

    fn input(name: &str) -> ModelPriceInput {
        ModelPriceInput {
            model_name: name.to_owned(),
            match_pattern: None,
            provider: None,
            valid_from: None,
            price_input: Some(1.0),
            price_output: Some(2.0),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: Vec::new(),
        }
    }

    fn tier(min: i64, price_input: Option<f64>) -> ModelPriceTier {
        ModelPriceTier {
            min_input_tokens: min,
            price_input,
            price_output: None,
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
        }
    }

    fn fields(errors: &[FieldError]) -> Vec<&str> {
        errors.iter().map(|error| error.field.as_str()).collect()
    }

    #[test]
    fn validate_accepts_a_minimal_row() {
        let body = ModelPriceInput {
            price_input: Some(0.0),
            price_output: Some(0.0),
            ..input(" llama3.1:8b ")
        };
        let row = validate_input(&body, PriceScope::Project).expect("valid");
        assert_eq!(row.model_name, "llama3.1:8b");
        assert_eq!(row.match_pattern, exact_pattern("llama3.1:8b"));
        assert_eq!(row.provider, None);
        assert_eq!(row.valid_from, 0);
        assert_eq!(row.tiers, None);
        assert_eq!(row.scope, PriceScope::Project);
    }

    #[test]
    fn validate_reports_every_field() {
        let body = ModelPriceInput {
            model_name: "  ".to_owned(),
            match_pattern: Some("a".repeat(501)),
            provider: Some("OpenAI".to_owned()),
            valid_from: Some(-1),
            price_input: Some(-1.0),
            price_output: Some(1e7),
            tiers: vec![tier(0, None), tier(0, Some(1.0))],
            ..input("unused")
        };
        let errors = validate_input(&body, PriceScope::Org).expect_err("invalid");
        assert_eq!(
            fields(&errors),
            [
                "model_name",
                "match_pattern",
                "provider",
                "valid_from",
                "price_input",
                "price_output",
                "tiers[0].min_input_tokens",
                "tiers[0]",
                "tiers[1].min_input_tokens",
            ]
        );
    }

    #[test]
    fn validate_refuses_a_pattern_that_only_works_wrapped() {
        let body = ModelPriceInput {
            match_pattern: Some("x)|(.*".to_owned()),
            ..input("gpt")
        };
        let errors = validate_input(&body, PriceScope::Project).expect_err("invalid");
        assert_eq!(fields(&errors), ["match_pattern"]);
    }

    #[test]
    fn validate_needs_input_or_output() {
        let body = ModelPriceInput {
            price_input: None,
            price_output: None,
            price_cache_read: Some(0.5),
            ..input("gpt")
        };
        let errors = validate_input(&body, PriceScope::Project).expect_err("invalid");
        assert_eq!(fields(&errors), ["price_input"]);
    }

    #[test]
    fn validate_allows_eight_tiers_no_more() {
        let eight: Vec<ModelPriceTier> = (1..=8).map(|min| tier(min, Some(1.0))).collect();
        let body = ModelPriceInput {
            tiers: eight,
            ..input("gpt")
        };
        assert!(validate_input(&body, PriceScope::Project).is_ok());
        let nine: Vec<ModelPriceTier> = (1..=9).map(|min| tier(min, Some(1.0))).collect();
        let body = ModelPriceInput {
            tiers: nine,
            ..input("gpt")
        };
        let errors = validate_input(&body, PriceScope::Project).expect_err("invalid");
        assert_eq!(fields(&errors), ["tiers"]);
    }

    #[test]
    fn validate_provider_rules() {
        for provider in ["gcp.vertex_ai", "x_ai", "azure.ai.openai"] {
            let body = ModelPriceInput {
                provider: Some(provider.to_owned()),
                ..input("gpt")
            };
            assert!(
                validate_input(&body, PriceScope::Project).is_ok(),
                "{provider}"
            );
        }
        let long = "a".repeat(65);
        for provider in [".x", "a b", long.as_str()] {
            let body = ModelPriceInput {
                provider: Some(provider.to_owned()),
                ..input("gpt")
            };
            let errors = validate_input(&body, PriceScope::Project).expect_err("invalid");
            assert_eq!(fields(&errors), ["provider"], "{provider}");
        }
        let blank = ModelPriceInput {
            provider: Some("  ".to_owned()),
            ..input("gpt")
        };
        let row = validate_input(&blank, PriceScope::Project).expect("valid");
        assert_eq!(row.provider, None);
    }

    #[test]
    fn record_and_builtin_json_round_trip_the_prices() {
        let row = PriceRow {
            scope: PriceScope::Project,
            model_name: "gpt-4o".to_owned(),
            match_pattern: "gpt\\-4o".to_owned(),
            provider: Some("openai".to_owned()),
            valid_from: 5,
            price_input: Some(2.0),
            price_output: Some(8.0),
            price_cache_read: Some(1.0),
            price_cache_write: None,
            price_reasoning: None,
            tiers: Some(vec![PriceTier {
                min_input_tokens: 100,
                price_input: Some(3.0),
                price_output: None,
                price_cache_read: None,
                price_cache_write: None,
                price_reasoning: None,
            }]),
        };
        let record = record(OrgId::new_v7(), Some(ProjectId::new_v7()), row.clone());
        let json = record_json(&record);
        assert_eq!(json.id, Some(record.id));
        assert_eq!(json.scope, ModelPriceScope::Project);
        assert_eq!(json.created_at, Some(0));
        assert_eq!(json.price_input, Some(2.0));
        assert_eq!(json.price_cache_read, Some(1.0));
        assert_eq!(json.provider.as_deref(), Some("openai"));
        assert_eq!(json.tiers.len(), 1);
        assert_eq!(json.tiers[0].min_input_tokens, 100);
        assert_eq!(json.tiers[0].price_input, Some(3.0));

        let built = builtin_json(&PriceRow {
            scope: PriceScope::Builtin,
            ..row
        });
        assert_eq!(built.id, None);
        assert_eq!(built.created_at, None);
        assert_eq!(built.scope, ModelPriceScope::Builtin);
        assert_eq!(built.price_output, Some(8.0));
    }
}
