//! Device authorization request and response types (RFC 8628, D-AO-3).
//!
//! Spec 08 § Device authorization. The codes are plaintext only in the answer that issues
//! them and in the grant; `Debug` never prints them.

use std::fmt;

use cetra_core::ids::ProjectId;
use cetra_core::time_serde;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Who asks for a code: `cetra login`, or `cetra-cli init` (or `cetra-cli login`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DevicePurpose {
    /// A personal access token that reads and writes.
    Login,
    /// A read-only personal access token and an ingest-only key for one project.
    Init,
}

/// `POST /api/auth/device/code` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceCodeRequest {
    /// What the code is for.
    pub purpose: DevicePurpose,
    /// The machine's name as the CLI reports it. Shown on the approval page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// `POST /api/auth/device/code` answer. `Debug` prints `device_code` as `<redacted>`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeviceCodeResponse {
    /// The secret the CLI polls with. Shown only in this answer.
    pub device_code: String,
    /// The code the user types, `XXXX-XXXX`.
    pub user_code: String,
    /// The approval page, without the code.
    pub verification_uri: String,
    /// The approval page with the code filled in.
    pub verification_uri_complete: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
    /// Seconds between polls.
    pub interval: u64,
}

impl fmt::Debug for DeviceCodeResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceCodeResponse")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// `POST /api/auth/device/token` body. `Debug` prints `device_code` as `<redacted>`.
#[derive(Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceTokenRequest {
    /// The `device_code` of `POST /api/auth/device/code`.
    pub device_code: String,
}

impl fmt::Debug for DeviceTokenRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceTokenRequest")
            .field("device_code", &"<redacted>")
            .finish()
    }
}

/// The project an `init` grant is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeviceGrantProject {
    /// Project id.
    pub id: ProjectId,
    /// Slug of the project's org.
    pub org_slug: String,
    /// Project slug.
    pub slug: String,
    /// Project name.
    pub name: String,
}

/// `POST /api/auth/device/token` answer once the user approved. The answer is the only place
/// the secrets appear. `Debug` prints `token` and `ingest_key` as `<redacted>`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeviceGrant {
    /// The server's public URL origin: what the CLI and the SDKs talk to.
    pub server: String,
    /// A personal access token, `pat-cetra-…`, valid for 90 days.
    pub token: String,
    /// The project of an `init` grant. Absent for `login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<DeviceGrantProject>,
    /// A secret key with the `ingest` scope only, `sk-cetra-…`. Present for `init`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_key: Option<String>,
}

impl fmt::Debug for DeviceGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceGrant")
            .field("server", &self.server)
            .field("token", &"<redacted>")
            .field("project", &self.project)
            .field(
                "ingest_key",
                &self.ingest_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `GET /api/auth/device/{user_code}` answer: what the approval page shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeviceRequest {
    /// What the code is for.
    pub purpose: DevicePurpose,
    /// The machine's name the CLI reported, when it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// When the code was issued. JSON is RFC 3339.
    #[serde(with = "time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
}

/// `POST /api/auth/device/{user_code}/approve` body. `project_id` is required for `init` and
/// refused for `login`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceApproveRequest {
    /// The project the `init` grant is for; the caller needs the admin role on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}

/// `POST /api/internal/v1/device/codes` body: the control plane registers a code it issued
/// (D-AO-14). `Debug` is derived: the hash is not a secret, since the device code cannot be
/// recovered from it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InternalDeviceCode {
    /// The lowercase hex SHA-256 of the device code: 64 characters.
    pub device_code_sha256: String,
    /// The user code the user types, `XXXX-XXXX`.
    pub user_code: String,
    /// What the code is for.
    pub purpose: DevicePurpose,
    /// The machine's name the CLI reported. Sanitised as `DeviceCodeRequest.host` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// When the code expires. JSON is RFC 3339.
    #[serde(with = "time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: i64,
}
