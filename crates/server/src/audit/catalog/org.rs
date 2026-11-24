//! `org.*` actions. `Dropped` is an instance record that names the org as its target.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum OrgAction {
    #[serde(rename = "org.created")]
    Created,
    /// Names are never recorded.
    #[serde(rename = "org.renamed")]
    Renamed,
    #[serde(rename = "org.entitlements_updated")]
    #[serde(rename_all = "snake_case")]
    EntitlementsUpdated { token_id: String },
    #[serde(rename = "org.dropped")]
    Dropped,
}
