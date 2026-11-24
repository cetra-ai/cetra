//! `pat.*` actions: personal access tokens. Instance records; the target is the token.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum PatAction {
    #[serde(rename = "pat.created")]
    #[serde(rename_all = "snake_case")]
    Created {
        scopes: Vec<&'static str>,
        expires_at: Option<i64>,
    },
    #[serde(rename = "pat.revoked")]
    Revoked,
}
