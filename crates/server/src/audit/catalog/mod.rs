//! The audit catalog (D-EA-4): one enum per name prefix, each in its own file.
//!
//! Names are dotted and append-only. A variant's `rename` is its stored name, so renaming
//! a variant in Rust does not change the record.

mod api_key;
mod auth;
mod deletion;
mod instance;
mod invite;
mod member;
mod org;
mod pat;
mod project;
mod sso;
mod user;

pub(crate) use api_key::ApiKeyAction;
pub(crate) use auth::{AuthAction, LoginMethod, SignupMethod};
pub(crate) use deletion::DeletionAction;
pub(crate) use instance::InstanceAction;
pub(crate) use invite::InviteAction;
pub(crate) use member::{JoinVia, MemberAction};
pub(crate) use org::OrgAction;
pub(crate) use pat::PatAction;
pub(crate) use project::ProjectAction;
pub(crate) use sso::SsoAction;
pub(crate) use user::UserAction;

/// Every audit action (D-EA-4). Serializes as `{"action": "<group>.<name>", "details": {…}}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum Action {
    Auth(AuthAction),
    Org(OrgAction),
    Project(ProjectAction),
    Member(MemberAction),
    Invite(InviteAction),
    ApiKey(ApiKeyAction),
    Pat(PatAction),
    Sso(SsoAction),
    User(UserAction),
    Instance(InstanceAction),
    Deletion(DeletionAction),
}

impl From<AuthAction> for Action {
    fn from(action: AuthAction) -> Self {
        Self::Auth(action)
    }
}

impl From<OrgAction> for Action {
    fn from(action: OrgAction) -> Self {
        Self::Org(action)
    }
}

impl From<ProjectAction> for Action {
    fn from(action: ProjectAction) -> Self {
        Self::Project(action)
    }
}

impl From<MemberAction> for Action {
    fn from(action: MemberAction) -> Self {
        Self::Member(action)
    }
}

impl From<InviteAction> for Action {
    fn from(action: InviteAction) -> Self {
        Self::Invite(action)
    }
}

impl From<ApiKeyAction> for Action {
    fn from(action: ApiKeyAction) -> Self {
        Self::ApiKey(action)
    }
}

impl From<PatAction> for Action {
    fn from(action: PatAction) -> Self {
        Self::Pat(action)
    }
}

impl From<SsoAction> for Action {
    fn from(action: SsoAction) -> Self {
        Self::Sso(action)
    }
}

impl From<UserAction> for Action {
    fn from(action: UserAction) -> Self {
        Self::User(action)
    }
}

impl From<InstanceAction> for Action {
    fn from(action: InstanceAction) -> Self {
        Self::Instance(action)
    }
}

impl From<DeletionAction> for Action {
    fn from(action: DeletionAction) -> Self {
        Self::Deletion(action)
    }
}
