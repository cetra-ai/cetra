//! `audit.*` actions: exports of the audit log itself. Filter values are never recorded,
//! only their names (track EA M3, D-EA-6).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum AuditAction {
    /// `format` is `csv` or `ndjson`. `filters` names the filters that were set.
    #[serde(rename = "audit.export_started")]
    #[serde(rename_all = "snake_case")]
    ExportStarted {
        format: &'static str,
        filters: Vec<&'static str>,
    },
}
