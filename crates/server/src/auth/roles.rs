//! Stored role strings. P10 adds the ceiling and override rules.
#![cfg_attr(not(test), allow(dead_code))]

pub(crate) use crate::api::account::OrgRole;

/// A per-project role override. `None` hides the project from that member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectOverride {
    /// Project admin.
    Admin,
    /// Project member.
    Member,
    /// Read-only on the project.
    Viewer,
    /// The project is hidden from this member.
    None,
}

/// The `org_members.role` text.
pub(crate) fn org_role_str(role: OrgRole) -> &'static str {
    match role {
        OrgRole::Owner => "owner",
        OrgRole::Admin => "admin",
        OrgRole::Member => "member",
        OrgRole::Viewer => "viewer",
    }
}

/// Parses `org_members.role`. An unknown string is `None` so the caller can
/// return [`crate::meta::MetaError::Corrupt`] without echoing the value.
pub(crate) fn parse_org_role(value: &str) -> Option<OrgRole> {
    match value {
        "owner" => Some(OrgRole::Owner),
        "admin" => Some(OrgRole::Admin),
        "member" => Some(OrgRole::Member),
        "viewer" => Some(OrgRole::Viewer),
        _ => None,
    }
}

/// The `project_members.role` text.
pub(crate) fn project_override_str(role: ProjectOverride) -> &'static str {
    match role {
        ProjectOverride::Admin => "admin",
        ProjectOverride::Member => "member",
        ProjectOverride::Viewer => "viewer",
        ProjectOverride::None => "none",
    }
}

/// Parses `project_members.role`. Unknown strings are `None`.
pub(crate) fn parse_project_override(value: &str) -> Option<ProjectOverride> {
    match value {
        "admin" => Some(ProjectOverride::Admin),
        "member" => Some(ProjectOverride::Member),
        "viewer" => Some(ProjectOverride::Viewer),
        "none" => Some(ProjectOverride::None),
        _ => None,
    }
}

/// Effective role on one project. `Viewer < Member < Admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Role {
    /// Read-only.
    Viewer,
    /// Member.
    Member,
    /// Project admin.
    Admin,
}

/// What an org route requires. Later tracks add the rows this enum does not cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrgPerm {
    /// View the org and its projects.
    View,
    /// Rename the org.
    Rename,
    /// Create a project.
    CreateProject,
    /// Members, invites, and org limits.
    ManageMembers,
    /// Create, edit, delete and test LLM connections (D-I-24).
    ManageConnections,
    /// Create, replace and delete the org's model prices (D-T-6).
    ManagePrices,
    /// Delete the org (owner only, spec 08).
    Delete,
    /// The org's deletion history (D-M-8).
    ViewDeletions,
    /// The org's identity provider, domains and enforcement (owner only, D-P-51).
    ManageSso,
    /// The org's SCIM settings and tokens (owner only, D-EA-15).
    ManageProvisioning,
}

/// What a project route requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectPerm {
    /// Read project data.
    View,
    /// Project settings, keys, and retention.
    ManageSettings,
    /// Project member overrides.
    ManageMembers,
    /// The org's member list as seen from a project: names, emails and roles. An API key never
    /// reads it (D-P-67).
    ViewMembers,
    /// Write and delete scores (spec 08: member and above).
    Annotate,
    /// Evaluators, online rules, score definitions, test and manual runs (D-I-24).
    ManageEvaluation,
    /// Alert rules and notification channels (spec 08: member and above; D-K-9).
    ManageAlerts,
    /// Datasets and experiments: create, edit, delete, items, import, add from trace, experiment
    /// results (D-J-5). Member and above; a `write` API key; a read-write personal access token.
    ManageDatasets,
    /// Prompts: create, version, label (unprotected; D-V-10 for protected ones), edit, delete
    /// (D-V-13). Member and above; a `write` API key; a read-write personal access token.
    ManagePrompts,
    /// Run the playground (D-V-26): member and above, a session or a read-write personal access
    /// token; never an API key.
    RunPlayground,
    /// Create, edit and delete dashboards (D-C-16). Member and above; no API key; a read-only
    /// personal access token cannot write (the access layer).
    ManageDashboards,
    /// Annotation queues: create, edit, delete, add and remove items (D-AQ-11). Member and above;
    /// a `write` API key; a read-write personal access token.
    ManageQueues,
    /// Work annotation queue items: next, lease, complete, skip, reopen (D-AQ-11). Member and
    /// above; never an API key, because a lease names a person.
    Review,
    /// Read project data through a POST with a body, such as the SQL console (D-H-4).
    /// Same holders as `View`; a read-only PAT and a `read` key may use it on POST.
    Query,
    /// GDPR deletions of traces and end users, and their history (D-M-4).
    Delete,
    /// Delete the project. The handler also requires org admin (spec 08, D-M-4).
    Drop,
    /// Chat with the in-app assistant and manage one's own conversations (D-AS-4).
    UseAssistant,
}

/// Scope on a project API key. Track D enforces [`Self::Read`] (D140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyScope {
    /// OTLP and score writes.
    Ingest,
    /// Safe methods of project routes.
    Read,
    /// Datasets, experiments, scores, and trace deletion.
    Write,
    /// Public feedback.
    Feedback,
}

impl KeyScope {
    fn bit(self) -> u8 {
        match self {
            Self::Ingest => 1,
            Self::Read => 2,
            Self::Write => 4,
            Self::Feedback => 8,
        }
    }
}

/// Scopes stored on one key. The order of [`Self::iter`] is stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyScopes(u8);

impl KeyScopes {
    /// The scopes in `scopes`. Duplicates collapse.
    pub(crate) fn from_scopes(scopes: &[KeyScope]) -> Self {
        let mut bits = 0;
        for scope in scopes {
            bits |= scope.bit();
        }
        Self(bits)
    }

    /// Whether `scope` is set.
    pub(crate) fn contains(self, scope: KeyScope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// `ingest`, `read`, `write`, `feedback`, skipping the ones that are unset.
    pub(crate) fn iter(self) -> impl Iterator<Item = KeyScope> {
        [
            KeyScope::Ingest,
            KeyScope::Read,
            KeyScope::Write,
            KeyScope::Feedback,
        ]
        .into_iter()
        .filter(move |scope| self.contains(*scope))
    }
}

/// Owner and admin are always project admins. An override applies to members and viewers.
/// `ProjectOverride::None` hides the project. No stored override keeps the org role.
pub(crate) fn effective_project_role(org: OrgRole, ov: Option<ProjectOverride>) -> Option<Role> {
    match org {
        OrgRole::Owner | OrgRole::Admin => Some(Role::Admin),
        OrgRole::Member => member_role(Role::Member, ov),
        OrgRole::Viewer => member_role(Role::Viewer, ov),
    }
}

/// `viewer < member < admin < owner`.
pub(crate) fn org_rank(role: OrgRole) -> u8 {
    match role {
        OrgRole::Viewer => 0,
        OrgRole::Member => 1,
        OrgRole::Admin => 2,
        OrgRole::Owner => 3,
    }
}

impl OrgPerm {
    /// Lowest org role that may perform this.
    pub(crate) fn min_role(self) -> OrgRole {
        match self {
            Self::View => OrgRole::Viewer,
            Self::Rename
            | Self::CreateProject
            | Self::ManageMembers
            | Self::ManageConnections
            | Self::ManagePrices
            | Self::ViewDeletions => OrgRole::Admin,
            Self::Delete | Self::ManageSso | Self::ManageProvisioning => OrgRole::Owner,
        }
    }
}

impl ProjectPerm {
    /// Lowest effective project role that may perform this.
    pub(crate) fn min_role(self) -> Role {
        match self {
            Self::View | Self::Query | Self::ViewMembers => Role::Viewer,
            Self::Annotate
            | Self::ManageEvaluation
            | Self::ManageAlerts
            | Self::ManageDatasets
            | Self::ManagePrompts
            | Self::RunPlayground
            | Self::ManageDashboards
            | Self::ManageQueues
            | Self::Review
            | Self::UseAssistant => Role::Member,
            Self::ManageSettings | Self::ManageMembers | Self::Delete | Self::Drop => Role::Admin,
        }
    }

    /// Scope an API key needs. `None` means a key cannot call the route.
    pub(crate) fn key_scope(self) -> Option<KeyScope> {
        match self {
            Self::View | Self::Query => Some(KeyScope::Read),
            Self::Annotate
            | Self::Delete
            | Self::ManageDatasets
            | Self::ManagePrompts
            | Self::ManageQueues => Some(KeyScope::Write),
            Self::ManageEvaluation
            | Self::ManageSettings
            | Self::ManageMembers
            | Self::ManageAlerts
            | Self::ManageDashboards
            | Self::Review
            | Self::RunPlayground
            | Self::ViewMembers
            | Self::Drop
            | Self::UseAssistant => None,
        }
    }
}

fn member_role(fallback: Role, ov: Option<ProjectOverride>) -> Option<Role> {
    match ov {
        None => Some(fallback),
        Some(ProjectOverride::None) => None,
        Some(ProjectOverride::Admin) => Some(Role::Admin),
        Some(ProjectOverride::Member) => Some(Role::Member),
        Some(ProjectOverride::Viewer) => Some(Role::Viewer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_role_covers_every_org_role_and_override() {
        let overrides = [
            None,
            Some(ProjectOverride::Admin),
            Some(ProjectOverride::Member),
            Some(ProjectOverride::Viewer),
            Some(ProjectOverride::None),
        ];
        for org in [
            OrgRole::Owner,
            OrgRole::Admin,
            OrgRole::Member,
            OrgRole::Viewer,
        ] {
            for ov in overrides {
                let role = effective_project_role(org, ov);
                let expected = match org {
                    OrgRole::Owner | OrgRole::Admin => Some(Role::Admin),
                    OrgRole::Member => member_role(Role::Member, ov),
                    OrgRole::Viewer => member_role(Role::Viewer, ov),
                };
                assert_eq!(role, expected, "{org:?} {ov:?}");
            }
        }
    }

    #[test]
    fn permissions_follow_the_spec_table() {
        // Spec 08 table: viewing is viewer; project settings and member management are admin.
        // Only transfer ownership is not a permission yet.
        assert_eq!(OrgPerm::View.min_role(), OrgRole::Viewer);
        assert_eq!(OrgPerm::Rename.min_role(), OrgRole::Admin);
        assert_eq!(OrgPerm::CreateProject.min_role(), OrgRole::Admin);
        assert_eq!(OrgPerm::ManageMembers.min_role(), OrgRole::Admin);
        assert_eq!(OrgPerm::ManageConnections.min_role(), OrgRole::Admin);
        assert_eq!(OrgPerm::ManagePrices.min_role(), OrgRole::Admin);
        assert_eq!(OrgPerm::Delete.min_role(), OrgRole::Owner);
        assert_eq!(OrgPerm::ViewDeletions.min_role(), OrgRole::Admin);
        assert_eq!(OrgPerm::ManageSso.min_role(), OrgRole::Owner);
        assert_eq!(OrgPerm::ManageProvisioning.min_role(), OrgRole::Owner);
        assert_eq!(ProjectPerm::View.min_role(), Role::Viewer);
        assert_eq!(ProjectPerm::ManageSettings.min_role(), Role::Admin);
        assert_eq!(ProjectPerm::ManageMembers.min_role(), Role::Admin);
        assert_eq!(ProjectPerm::View.key_scope(), Some(KeyScope::Read));
        assert_eq!(ProjectPerm::Query.min_role(), Role::Viewer);
        assert_eq!(ProjectPerm::Query.key_scope(), Some(KeyScope::Read));
        assert_eq!(ProjectPerm::Delete.min_role(), Role::Admin);
        assert_eq!(ProjectPerm::Delete.key_scope(), Some(KeyScope::Write));
        assert_eq!(ProjectPerm::Drop.min_role(), Role::Admin);
        assert!(ProjectPerm::Drop.key_scope().is_none());
        assert!(ProjectPerm::ManageSettings.key_scope().is_none());
        assert!(ProjectPerm::ManageMembers.key_scope().is_none());
        assert_eq!(ProjectPerm::ViewMembers.min_role(), Role::Viewer);
        assert!(ProjectPerm::ViewMembers.key_scope().is_none());
        // Score writes: viewer no; member, admin and owner yes.
        assert_eq!(ProjectPerm::Annotate.min_role(), Role::Member);
        assert_eq!(ProjectPerm::Annotate.key_scope(), Some(KeyScope::Write));
        // Evaluation: member and above; an API key cannot manage it (D-I-24).
        assert_eq!(ProjectPerm::ManageEvaluation.min_role(), Role::Member);
        assert!(ProjectPerm::ManageEvaluation.key_scope().is_none());
        assert_eq!(ProjectPerm::ManageAlerts.min_role(), Role::Member);
        assert!(ProjectPerm::ManageAlerts.key_scope().is_none());
        // Datasets and experiments: member and above, and a `write` key (D-J-5).
        assert_eq!(ProjectPerm::ManageDatasets.min_role(), Role::Member);
        assert_eq!(
            ProjectPerm::ManageDatasets.key_scope(),
            Some(KeyScope::Write)
        );
        // Prompts: member and above, and a `write` key (D-V-13).
        assert_eq!(ProjectPerm::ManagePrompts.min_role(), Role::Member);
        assert_eq!(
            ProjectPerm::ManagePrompts.key_scope(),
            Some(KeyScope::Write)
        );
        // Playground: member and above; an API key cannot run it (D-V-26).
        assert_eq!(ProjectPerm::RunPlayground.min_role(), Role::Member);
        assert!(ProjectPerm::RunPlayground.key_scope().is_none());
        // Dashboards: member and above; an API key cannot manage them (D-C-16).
        assert_eq!(ProjectPerm::ManageDashboards.min_role(), Role::Member);
        assert!(ProjectPerm::ManageDashboards.key_scope().is_none());
        // Annotation queues: managing them is member and above with a `write` key (D-AQ-11);
        // a review names a person, so no key may review.
        assert_eq!(ProjectPerm::ManageQueues.min_role(), Role::Member);
        assert_eq!(ProjectPerm::ManageQueues.key_scope(), Some(KeyScope::Write));
        assert_eq!(ProjectPerm::Review.min_role(), Role::Member);
        assert!(ProjectPerm::Review.key_scope().is_none());
        // Assistant conversations: member and above; an API key cannot use them (D-AS-4).
        assert_eq!(ProjectPerm::UseAssistant.min_role(), Role::Member);
        assert!(ProjectPerm::UseAssistant.key_scope().is_none());
        let viewer =
            effective_project_role(OrgRole::Viewer, None).expect("viewer sees the project");
        assert!(viewer < ProjectPerm::Annotate.min_role());
        for org in [OrgRole::Member, OrgRole::Admin, OrgRole::Owner] {
            let role = effective_project_role(org, None).expect("member sees the project");
            assert!(role >= ProjectPerm::Annotate.min_role(), "{org:?}");
        }
        assert_eq!(org_rank(OrgRole::Viewer), 0);
        assert!(org_rank(OrgRole::Viewer) < org_rank(OrgRole::Member));
        assert!(org_rank(OrgRole::Member) < org_rank(OrgRole::Admin));
        assert!(org_rank(OrgRole::Admin) < org_rank(OrgRole::Owner));
        let _scopes = [
            KeyScope::Ingest,
            KeyScope::Read,
            KeyScope::Write,
            KeyScope::Feedback,
        ];
    }
}
