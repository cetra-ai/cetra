//! `instance.*` actions. Instance records (no org).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum InstanceAction {
    /// Actor `system`.
    #[serde(rename = "instance.init_applied")]
    InitApplied,
    /// Target `license`. The key itself is never recorded.
    #[serde(rename = "instance.license_installed")]
    #[serde(rename_all = "snake_case")]
    LicenseInstalled { license_id: String, kid: String },
}
