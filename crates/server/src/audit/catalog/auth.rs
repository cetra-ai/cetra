//! `auth.*` actions: sign-ins, sign-ups, passwords and devices (A2 records them).

/// How a user signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoginMethod {
    Password,
    Provider,
    OrgProvider,
}

/// How an account was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignupMethod {
    Password,
    Invitation,
    Provider,
    OrgProvider,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum AuthAction {
    #[serde(rename = "auth.login")]
    #[serde(rename_all = "snake_case")]
    Login { method: LoginMethod },
    #[serde(rename = "auth.login_failed")]
    LoginFailed,
    #[serde(rename = "auth.logout")]
    Logout,
    #[serde(rename = "auth.signup")]
    #[serde(rename_all = "snake_case")]
    Signup { method: SignupMethod },
    #[serde(rename = "auth.email_verified")]
    EmailVerified,
    #[serde(rename = "auth.password_changed")]
    PasswordChanged,
    #[serde(rename = "auth.password_reset_requested")]
    PasswordResetRequested,
    #[serde(rename = "auth.password_reset")]
    PasswordReset,
    #[serde(rename = "auth.identity_linked")]
    #[serde(rename_all = "snake_case")]
    IdentityLinked { provider: String },
    #[serde(rename = "auth.setup_completed")]
    SetupCompleted,
    #[serde(rename = "auth.device_approved")]
    #[serde(rename_all = "snake_case")]
    DeviceApproved {
        purpose: &'static str,
        token_id: uuid::Uuid,
        key_id: Option<uuid::Uuid>,
    },
    #[serde(rename = "auth.device_denied")]
    DeviceDenied,
    #[serde(rename = "auth.device_registered")]
    #[serde(rename_all = "snake_case")]
    DeviceRegistered { purpose: &'static str },
}
