//! `sql.*` actions: SQL console runs and exports. The query text is never recorded; only its
//! lowercase hex SHA-256. Written before any data is read (D-EA-6). Demo-org visitors are not
//! recorded (D-EA-23).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum SqlAction {
    #[serde(rename = "sql.run")]
    #[serde(rename_all = "snake_case")]
    Run { query_sha256: String },
    /// `format` is `csv`, `ndjson` or `parquet`.
    #[serde(rename = "sql.export_started")]
    #[serde(rename_all = "snake_case")]
    ExportStarted {
        query_sha256: String,
        format: &'static str,
    },
}
