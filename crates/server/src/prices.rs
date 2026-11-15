//! The price tables of every org and project (spec 02 § Cost computation, D-T-3, D-T-9).
//!
//! The built-in snapshot is compiled once. An org with org rows gets a table of those rows
//! layered on the built-in one; a project with rows of its own gets a table of the org rows and
//! its rows layered on the built-in one. The ingest path reads a map under a read lock and never
//! touches `meta.db`.

// T4 removes this line, once the write routes call `reload_org` and `lock_writes`.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use llmobs_core::ids::{OrgId, ProjectId};
use llmobs_otel::{PriceError, PriceRow, PriceTable};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, MutexGuard};

use crate::meta::MetaError;
use crate::meta::model_prices::{self, PriceRecord};

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

#[cfg(test)]
mod tests {
    use llmobs_otel::PriceScope;
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
}
