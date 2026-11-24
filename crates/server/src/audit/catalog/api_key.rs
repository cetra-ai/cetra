//! `api_key.*` actions: project API keys. The target is the key.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum ApiKeyAction {
    /// `kind` is `secret` or `public`; `scopes` are scope names.
    #[serde(rename = "api_key.created")]
    #[serde(rename_all = "snake_case")]
    Created {
        kind: &'static str,
        scopes: Vec<&'static str>,
    },
    #[serde(rename = "api_key.revoked")]
    Revoked,
}
