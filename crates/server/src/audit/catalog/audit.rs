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
    /// The org's or the instance's stream destination was created or changed (D-EA-11). `url_host`
    /// is the endpoint's host; `header` says whether a header is set.
    #[serde(rename = "audit.stream_set")]
    #[serde(rename_all = "snake_case")]
    StreamSet {
        url_host: String,
        header: bool,
        enabled: bool,
    },
    /// The stream destination was removed.
    #[serde(rename = "audit.stream_removed")]
    StreamRemoved,
    /// The stream's signing secret was rotated.
    #[serde(rename = "audit.stream_secret_rotated")]
    StreamSecretRotated,
}
