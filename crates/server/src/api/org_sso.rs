//! Per-org SSO types (track P M5, D-P-56). The settings routes are in `ee::org_sso::settings`.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::account::OrgRole;

/// Role given to people who join through the org's provider (D-P-56).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JitRole {
    /// Read-only.
    Viewer,
    /// Member.
    Member,
}

impl JitRole {
    /// The stored text (`org_sso_providers.jit_role`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Member => "member",
        }
    }

    /// The role named by a stored value. `None` for anything else.
    pub(crate) fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    /// The org role a joining user gets.
    pub(crate) fn org_role(self) -> OrgRole {
        match self {
            Self::Viewer => OrgRole::Viewer,
            Self::Member => OrgRole::Member,
        }
    }
}

/// The protocol of an org's provider (D-SA-2, D-SA-17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SsoProtocol {
    /// OpenID Connect.
    Oidc,
    /// SAML 2.0.
    Saml,
}

impl SsoProtocol {
    /// The stored text (`org_sso_providers.protocol`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::Saml => "saml",
        }
    }

    /// The protocol named by a stored value. `None` for anything else.
    pub(crate) fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "oidc" => Some(Self::Oidc),
            "saml" => Some(Self::Saml),
            _ => None,
        }
    }
}

/// The org's identity provider. The client secret is write-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSsoProvider {
    /// Shown on the login page.
    pub display_name: String,
    /// The OIDC issuer URL.
    pub issuer: String,
    /// The OAuth client id.
    pub client_id: String,
    /// Discovery sends users to this provider (D-P-57).
    pub enabled: bool,
    /// Password and other sign-ins are refused for the org's domains (D-P-58).
    pub enforced: bool,
    /// Role of people who join through the provider.
    pub jit_role: JitRole,
    /// The last successful sign-in through the provider. Absent before the first one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_login_at: Option<i64>,
    /// Creation time.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last change to the settings.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// Whether the org has proved the domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DomainStatus {
    /// The TXT record has not been found yet.
    Pending,
    /// The TXT record was found.
    Verified,
}

/// One email domain of the org and how to prove it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSsoDomain {
    /// The domain row id.
    pub id: Uuid,
    /// Lowercase domain name.
    pub domain: String,
    /// Pending or verified.
    pub status: DomainStatus,
    /// TXT record name, `_cetra-verify.<domain>`.
    pub txt_name: String,
    /// TXT record value, `cetra-verify=<token>`.
    pub txt_value: String,
    /// When the TXT record was found. Omitted while pending.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub verified_at: Option<i64>,
    /// Last lookup, found or not. Omitted before the first one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_checked_at: Option<i64>,
    /// Creation time.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
}

/// `GET /api/v1/orgs/{org}/sso` and every settings write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSso {
    /// The redirect URI to register at the identity provider.
    pub callback_url: String,
    /// The provider, when the org has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<OrgSsoProvider>,
    /// Ordered by domain.
    pub domains: Vec<OrgSsoDomain>,
}

/// `POST /api/auth/sso/discover`. `Debug` omits the email.
#[derive(Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SsoDiscoverRequest {
    /// The address the person typed on the login page.
    pub email: String,
}

impl fmt::Debug for SsoDiscoverRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SsoDiscoverRequest { .. }")
    }
}

/// Whether the email's domain signs in through an org's provider (D-P-60). A function of the
/// domain only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SsoDiscovery {
    /// True when the domain's org has an enabled provider.
    pub sso: bool,
}

/// `PATCH /api/v1/orgs/{org}/sso`: the flags, each optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchOrgSso {
    /// Discovery sends users to the provider (D-P-57).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Enforce SSO for the org's verified domains (D-P-58).
    #[serde(default)]
    pub enforced: Option<bool>,
}

/// `POST /api/v1/orgs/{org}/sso/domains`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddOrgSsoDomain {
    /// The email domain, e.g. `acme.com`. Punycode for internationalized names.
    pub domain: String,
}

/// `PUT /api/v1/orgs/{org}/sso/provider`. `Debug` omits the client secret.
#[derive(Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutOrgSsoProvider {
    /// Shown on the login page; 1 to 100 characters.
    pub display_name: String,
    /// The OIDC issuer URL; `https`, or `http` on a loopback host.
    pub issuer: String,
    /// The OAuth client id; 1 to 255 characters.
    pub client_id: String,
    /// Required when creating, and when the issuer or client id changes; omit to keep it.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Role of people who join through the provider.
    pub jit_role: JitRole,
}

impl fmt::Debug for PutOrgSsoProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PutOrgSsoProvider")
            .field("display_name", &self.display_name)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("jit_role", &self.jit_role)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_role_round_trips_its_stored_text() {
        for role in [JitRole::Viewer, JitRole::Member] {
            assert_eq!(JitRole::from_str_opt(role.as_str()), Some(role));
        }
        assert_eq!(JitRole::from_str_opt("owner"), None);
        assert_eq!(JitRole::from_str_opt("admin"), None);
        assert_eq!(JitRole::Viewer.org_role(), OrgRole::Viewer);
        assert_eq!(JitRole::Member.org_role(), OrgRole::Member);
    }

    #[test]
    fn the_put_body_debug_omits_the_secret() {
        let body: PutOrgSsoProvider = serde_json::from_str(
            r#"{"display_name":"Acme","issuer":"https://idp.example","client_id":"c",
                "client_secret":"s3cret-value","jit_role":"member"}"#,
        )
        .expect("body");
        let text = format!("{body:?}");
        assert!(!text.contains("s3cret-value"), "{text}");
        assert!(text.contains("Acme"), "{text}");
    }
}
