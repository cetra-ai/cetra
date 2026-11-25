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
    /// The stored text (`org_sso_providers.protocol`), and the audit details' name for it.
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
    /// OpenID Connect or SAML. The OIDC fields are set for an OIDC provider, `saml` for a SAML one.
    pub protocol: SsoProtocol,
    /// Shown on the login page.
    pub display_name: String,
    /// The OIDC issuer URL. Set for an OIDC provider only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub issuer: Option<String>,
    /// The OAuth client id. Set for an OIDC provider only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_id: Option<String>,
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
    /// The SAML settings. Set for a SAML provider only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub saml: Option<OrgSsoSaml>,
}

/// A SAML provider's settings (D-SA-11, D-SA-18). Never contains the metadata document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSsoSaml {
    /// The IdP's entity ID (the `Issuer` of its responses).
    pub idp_entity_id: String,
    /// The IdP's HTTP-Redirect sign-on URL.
    pub sso_url: String,
    /// The metadata URL, when the metadata was fetched from one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata_url: Option<String>,
    /// The trusted IdP signing certificates.
    pub certificates: Vec<OrgSsoCertificate>,
    /// The attribute names the owner set. An absent field uses the default.
    pub attributes: SamlAttributeNames,
    /// When the metadata was last fetched. Omitted for pasted metadata or manual fields.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub refreshed_at: Option<i64>,
    /// Why the last scheduled refresh failed.
    pub refresh_error: Option<SamlRefreshError>,
    /// When the set of certificates last changed. Omitted before the first change.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub certificates_changed_at: Option<i64>,
}

/// A trusted IdP signing certificate, shown by hash and validity (D-SA-11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSsoCertificate {
    /// The hex SHA-256 of the certificate's DER encoding.
    pub sha256: String,
    /// Start of validity.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub not_before: i64,
    /// End of validity.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub not_after: i64,
}

/// Attribute names an owner set; an absent field uses the defaults (D-SA-6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SamlAttributeNames {
    /// The attribute that carries the email address.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub email: Option<String>,
    /// The attribute that carries the display name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display_name: Option<String>,
    /// The attribute that carries the first name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_name: Option<String>,
    /// The attribute that carries the last name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_name: Option<String>,
}

/// Why the last scheduled metadata refresh failed (`org_sso_saml.refresh_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SamlRefreshError {
    /// The metadata could not be fetched.
    FetchFailed,
    /// The metadata URL answered with a status other than `200`.
    Status,
    /// The metadata is larger than the limit.
    TooLarge,
    /// The egress policy refused the metadata URL.
    Denied,
    /// The fetched metadata is not usable.
    Unusable,
    /// The fetched metadata has another entity ID; the change was not applied.
    EntityIdChanged,
}

impl SamlRefreshError {
    /// The stored text (`org_sso_saml.refresh_error`). `None` for anything else.
    pub(crate) fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "fetch_failed" => Some(Self::FetchFailed),
            "status" => Some(Self::Status),
            "too_large" => Some(Self::TooLarge),
            "denied" => Some(Self::Denied),
            "unusable" => Some(Self::Unusable),
            "entity_id_changed" => Some(Self::EntityIdChanged),
            _ => None,
        }
    }
}

/// The values an owner copies into the IdP (D-SA-12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSsoSamlSp {
    /// The SP entity ID.
    pub entity_id: String,
    /// The assertion consumer service URL.
    pub acs_url: String,
    /// Starts a login; also the IdP's tile URL.
    pub start_url: String,
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
    /// The values for the org's SAML provider (D-SA-12). The same for every protocol.
    pub saml_sp: OrgSsoSamlSp,
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

/// `PUT /api/v1/orgs/{org}/sso/saml` (D-SA-18). Exactly one source: `metadata_url`,
/// `metadata_xml`, or `idp_entity_id` + `sso_url` + `certificates`. With none of them, only the
/// settings of the org's SAML provider are saved. `Debug` omits the metadata and the fields.
#[derive(Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutOrgSamlProvider {
    /// Shown on the login page; 1 to 100 characters.
    pub display_name: String,
    /// Role of people who join through the provider.
    pub jit_role: JitRole,
    /// An `https` metadata URL, fetched now and refreshed daily.
    #[serde(default)]
    pub metadata_url: Option<String>,
    /// The IdP metadata document, pasted.
    #[serde(default)]
    pub metadata_xml: Option<String>,
    /// The IdP's entity ID, with `sso_url` and `certificates`.
    #[serde(default)]
    pub idp_entity_id: Option<String>,
    /// The IdP's HTTP-Redirect sign-on URL, with `idp_entity_id` and `certificates`.
    #[serde(default)]
    pub sso_url: Option<String>,
    /// PEM certificates of the IdP's signing keys, with `idp_entity_id` and `sso_url`.
    #[serde(default)]
    pub certificates: Option<Vec<String>>,
    /// Attribute names; an absent field uses the default.
    #[serde(default)]
    pub attributes: SamlAttributeNames,
    /// Needed to change the IdP entity ID, which deletes the linked identities.
    #[serde(default)]
    pub confirm_entity_id_change: bool,
}

impl fmt::Debug for PutOrgSamlProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PutOrgSamlProvider")
            .field("metadata_url", &self.metadata_url.is_some())
            .field("metadata_xml", &self.metadata_xml.is_some())
            .field("idp_entity_id", &self.idp_entity_id.is_some())
            .field("sso_url", &self.sso_url.is_some())
            .field("certificates", &self.certificates.as_ref().map(Vec::len))
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
