//! `dashboard.*` actions. The target is the dashboard (`target_kind` `dashboard`). Dashboards
//! are replaced as whole documents, so `updated` carries no field list.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum DashboardAction {
    #[serde(rename = "dashboard.created")]
    Created,
    #[serde(rename = "dashboard.updated")]
    Updated,
    #[serde(rename = "dashboard.deleted")]
    Deleted,
    /// `created` is true when the overview was created rather than replaced.
    #[serde(rename = "dashboard.overview_saved")]
    #[serde(rename_all = "snake_case")]
    OverviewSaved { created: bool },
    #[serde(rename = "dashboard.overview_reset")]
    OverviewReset,
}
