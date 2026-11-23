//! Audit record and export request types (track EA M3, D-EA-3, D-EA-20, plan P-1 to P-3).

use cetra_core::time_serde;
use serde::{Deserialize, Deserializer, Serialize, de};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::TimeParam;
use super::keys::optional_time;

/// Whether the action succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Success,
    Failure,
}

/// How a referenced user, key, project or provider resolves at read time (plan P-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditRefStatus {
    /// The object exists. Name, and email for a user, are shown.
    Present,
    /// A user who is no longer a member of the org. The org log shows no name or email.
    FormerMember,
    /// The object was deleted, or its user was erased.
    Deleted,
    /// A kind this scope does not resolve (an org object in the instance log).
    Unresolved,
}

/// The export format (plan step 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditExportFormat {
    /// Comma-separated values, one row per record.
    Csv,
    /// One JSON record per line.
    Ndjson,
}

/// The actor of a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuditActor {
    /// `user`, `api_key`, `scim_token`, `control_plane`, `cli`, `system` or `anonymous`.
    pub kind: String,
    /// The user, key, token id. Omitted when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `session` or `pat` for a user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The personal access token id, when a user acted through one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// The display name, when it resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The email, for a present user only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub status: AuditRefStatus,
}

/// An object a record names: its target, or the project of a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuditRef {
    /// The target kind, as the record stores it.
    pub kind: String,
    pub id: String,
    /// The display name, when it resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The email, for a present user only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub status: AuditRefStatus,
}

/// One audit record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuditEvent {
    /// Record id.
    pub id: Uuid,
    /// When the action happened.
    #[serde(with = "time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub at: i64,
    /// The action name, `group.name`.
    pub action: String,
    pub outcome: AuditOutcome,
    pub actor: AuditActor,
    /// The object the action changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AuditRef>,
    /// The project the action belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<AuditRef>,
    /// The action's details: ids, names of changed fields, roles, counts. Never payload.
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
    /// The client IP, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// The client's user agent, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

/// Page size of the audit list: 1 to 500, default 100 (plan P-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = u32, minimum = 1, maximum = 500, default = 100)]
pub struct AuditLimit(pub u32);

impl AuditLimit {
    /// The page size.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for AuditLimit {
    fn default() -> Self {
        Self(100)
    }
}

impl<'de> Deserialize<'de> for AuditLimit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u32::deserialize(deserializer)?;
        if value == 0 || value > 500 {
            return Err(de::Error::custom("limit must be between 1 and 500"));
        }
        Ok(Self(value))
    }
}

/// Filters and paging of the audit list (plan P-1, P-2).
///
/// `from` is inclusive and `to` exclusive. `action` is a comma-separated list; an entry ending
/// in `.` names a group (`member.` matches every `member.*` action).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct AuditListParams {
    /// Records at or after this time (RFC 3339 or epoch milliseconds).
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Records before this time (RFC 3339 or epoch milliseconds).
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Comma-separated action names or group prefixes, at most 20 entries.
    #[serde(default)]
    pub action: Option<String>,
    /// The actor's id: user, API key or token id. At most 64 characters.
    #[serde(default)]
    pub actor_id: Option<String>,
    /// The target's id. At most 64 characters.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Records of this project. The instance log has none, so it matches nothing there.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// `success` or `failure`.
    #[serde(default)]
    pub outcome: Option<AuditOutcome>,
    /// Maximum number of records (1–500, default 100).
    #[serde(default)]
    #[param(minimum = 1, maximum = 500, default = 100)]
    pub limit: AuditLimit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Filters of an export: the list's filters, without paging, and the format.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct AuditExportParams {
    /// Records at or after this time (RFC 3339 or epoch milliseconds).
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Records before this time (RFC 3339 or epoch milliseconds).
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Comma-separated action names or group prefixes, at most 20 entries.
    #[serde(default)]
    pub action: Option<String>,
    /// The actor's id. At most 64 characters.
    #[serde(default)]
    pub actor_id: Option<String>,
    /// The target's id. At most 64 characters.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Records of this project.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// `success` or `failure`.
    #[serde(default)]
    pub outcome: Option<AuditOutcome>,
    /// `csv` or `ndjson`.
    pub format: AuditExportFormat,
}

/// The SIEM stream destination of an org or of the instance (track EA M4, D-EA-11). The URL,
/// the signing secret and the header value are never returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditStream {
    /// The endpoint's host.
    pub url_host: String,
    /// The optional header's name. Its value is never returned.
    pub header_name: Option<String>,
    /// Whether records are sent.
    pub enabled: bool,
    pub status: AuditStreamStatus,
}

/// How the stream is doing. Records are sent oldest first, at least once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditStreamStatus {
    /// The last delivery the endpoint accepted. Null until the first one.
    #[serde(default, with = "optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_success_at: Option<i64>,
    /// The class of the last failure, when the last attempt failed. Never a URL or a value.
    pub last_error: Option<String>,
    /// When the current run of failures began. Null when the last attempt worked.
    #[serde(default, with = "optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub failing_since: Option<i64>,
    /// Records not yet accepted by the endpoint.
    pub lag: u64,
    /// Whether retention deleted records before they were sent.
    pub lost_records: bool,
}

/// `PUT` body of the stream. The URL must be https and carry no user information; the optional
/// header is one name and one value, sealed and never returned. Without `header_value`, a
/// stored header keeps its value when the name is unchanged.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutAuditStream {
    /// The endpoint, an https URL of at most 2,048 bytes. Never returned.
    pub url: String,
    /// The optional header's name, for example `Authorization`.
    #[serde(default)]
    pub header_name: Option<String>,
    /// The header's value, at most 1,024 bytes. Never returned.
    #[serde(default)]
    pub header_value: Option<String>,
    /// Whether records are sent.
    pub enabled: bool,
}

/// `201` of the create: the destination, and its signing secret, which is shown only here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreatedAuditStream {
    pub stream: AuditStream,
    /// The signing secret: verify `X-Cetra-Signature` with it. Shown once.
    pub secret: String,
}

/// `POST …/secret`: the new signing secret, shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RotatedAuditStreamSecret {
    pub secret: String,
}

/// The answer to a test send. Only reachability: connection errors and timeouts both read as
/// not reached (plan P-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditStreamTest {
    /// Whether the endpoint answered.
    pub reached: bool,
    /// The HTTP status it answered with, when it answered.
    pub status: Option<u16>,
}
