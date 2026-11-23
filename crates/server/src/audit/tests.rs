//! Tests for the audit core: the table, `record`, the throttle, the `Ctx` extractor and the
//! catalog's names and details.

use std::collections::HashSet;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::USER_AGENT;
use cetra_core::ids::{OrgId, ProjectId};
use sqlx::Row;
use uuid::Uuid;

use super::catalog::{
    AlertChannelAction, AlertRuleAction, AnnotationQueueAction, ApiKeyAction, AssistantAction,
    AuditAction, AuthAction, DashboardAction, DatasetAction, DeletionAction, EvaluatorAction,
    InstanceAction, InviteAction, IssueSourceAction, JoinVia, LlmConnectionAction, LoginMethod,
    McpAction, MemberAction, ModelPriceAction, OnlineRuleAction, OrgAction, PatAction,
    ProjectAction, PromptAction, ScimAction, ScoreDefinitionAction, SignupMethod, SqlAction,
    SsoAction, UserAction,
};
use super::{
    Action, Actor, Ctx, Event, LOGIN_FAILED_WINDOW_NS, TargetKind, Via, record, record_alone,
    record_login_failed, scope_of_rule, sha256_hex,
};
use crate::auth::principal::{AuthUser, Principal};
use crate::auth::roles::{KeyScope, KeyScopes};
use crate::auth::secret::KeyKind;
use crate::http::AppState;
use crate::meta::connections::{ConnectionFields, insert_connection};
use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
use crate::meta::orgs::insert_org;
use crate::meta::projects::insert_project;
use crate::meta::{MetaDb, MetaError};
use crate::test_support::{TestApp, capture_logs};

const SECOND: i64 = 1_000_000_000;

/// One value of every catalog variant.
fn samples() -> Vec<Action> {
    let uuid = Uuid::from_u128(1);
    vec![
        AuthAction::Login {
            method: LoginMethod::Password,
        }
        .into(),
        AuthAction::LoginFailed.into(),
        AuthAction::Logout.into(),
        AuthAction::Signup {
            method: SignupMethod::Invitation,
        }
        .into(),
        AuthAction::EmailVerified.into(),
        AuthAction::PasswordChanged.into(),
        AuthAction::PasswordResetRequested.into(),
        AuthAction::PasswordReset.into(),
        AuthAction::IdentityLinked {
            provider: "github".to_owned(),
        }
        .into(),
        AuthAction::SetupCompleted.into(),
        AuthAction::DeviceApproved {
            purpose: "login",
            token_id: uuid,
            key_id: None,
        }
        .into(),
        AuthAction::DeviceDenied.into(),
        AuthAction::DeviceRegistered { purpose: "init" }.into(),
        OrgAction::Created.into(),
        OrgAction::Renamed.into(),
        OrgAction::EntitlementsUpdated {
            token_id: "dev-1".to_owned(),
        }
        .into(),
        OrgAction::Dropped.into(),
        ProjectAction::Created.into(),
        ProjectAction::Renamed.into(),
        ProjectAction::RetentionChanged {
            old_days: None,
            new_days: Some(30),
        }
        .into(),
        ProjectAction::CompletionTimeoutChanged {
            old_seconds: Some(60),
            new_seconds: None,
        }
        .into(),
        ProjectAction::Dropped.into(),
        MemberAction::Added {
            role: "viewer",
            via: JoinVia::SsoJit,
        }
        .into(),
        MemberAction::RoleChanged {
            old_role: "viewer",
            new_role: "admin",
        }
        .into(),
        MemberAction::Removed { role: "admin" }.into(),
        MemberAction::OverrideSet {
            project_id: uuid,
            old_role: None,
            new_role: "editor",
        }
        .into(),
        MemberAction::OverrideRemoved {
            project_id: uuid,
            old_role: Some("editor"),
        }
        .into(),
        InviteAction::Created { role: "viewer" }.into(),
        InviteAction::Revoked.into(),
        InviteAction::Accepted { role: "viewer" }.into(),
        ApiKeyAction::Created {
            kind: "secret",
            scopes: vec!["ingest", "read"],
        }
        .into(),
        ApiKeyAction::Revoked.into(),
        PatAction::Created {
            scopes: vec!["read"],
            expires_at: Some(1),
        }
        .into(),
        PatAction::Revoked.into(),
        SsoAction::ProviderSaved {
            issuer_changed: true,
        }
        .into(),
        SsoAction::ProviderRemoved.into(),
        SsoAction::SettingsChanged {
            enabled_before: false,
            enabled: true,
            enforced_before: false,
            enforced: true,
            revoked_users: 2,
        }
        .into(),
        SsoAction::DomainAdded.into(),
        SsoAction::DomainVerified {
            taken_from: Some(uuid),
        }
        .into(),
        SsoAction::DomainLost { to_org_id: uuid }.into(),
        SsoAction::DomainRemoved.into(),
        SsoAction::DomainLapsed.into(),
        SsoAction::Disabled.into(),
        ScimAction::Enabled.into(),
        ScimAction::Disabled.into(),
        ScimAction::DefaultRoleChanged {
            old_role: "viewer",
            new_role: "member",
        }
        .into(),
        ScimAction::TokenCreated.into(),
        ScimAction::TokenRevoked.into(),
        ScimAction::UserProvisioned {
            scim_user_id: uuid,
            role: "viewer",
            linked: true,
        }
        .into(),
        ScimAction::UserUpdated {
            scim_user_id: uuid,
            fields: vec!["department", "active"],
        }
        .into(),
        ScimAction::UserDeactivated {
            scim_user_id: uuid,
            account_disabled: false,
        }
        .into(),
        ScimAction::UserReactivated { scim_user_id: uuid }.into(),
        ScimAction::UserDeleted { scim_user_id: uuid }.into(),
        ScimAction::GroupCreated {
            scim_group_id: uuid,
            members: 2,
        }
        .into(),
        ScimAction::GroupUpdated {
            scim_group_id: uuid,
            fields: vec!["displayName", "externalId"],
        }
        .into(),
        ScimAction::GroupDeleted {
            scim_group_id: uuid,
        }
        .into(),
        ScimAction::GroupMembersChanged {
            scim_group_id: uuid,
            added: 1,
            removed: 0,
        }
        .into(),
        ScimAction::GroupMappingChanged {
            scim_group_id: uuid,
            org_role: Some("member"),
            projects: 1,
        }
        .into(),
        ScimAction::RolesRecomputed {
            users: 3,
            changed: 1,
        }
        .into(),
        ScimAction::RoleRaiseRefused {
            count: 1,
            scim_user_ids: vec![uuid],
        }
        .into(),
        UserAction::Created {
            instance_admin: false,
        }
        .into(),
        UserAction::Updated {
            disabled: Some(true),
            instance_admin: None,
        }
        .into(),
        UserAction::ResetLinkIssued.into(),
        UserAction::SessionsRevoked.into(),
        UserAction::Disabled.into(),
        UserAction::Deleted { sole_owner_orgs: 0 }.into(),
        InstanceAction::InitApplied.into(),
        InstanceAction::LicenseInstalled {
            license_id: "lic-1".to_owned(),
            kid: "kid-1".to_owned(),
        }
        .into(),
        DeletionAction::Requested {
            kind: "traces",
            follow_up_of: None,
        }
        .into(),
        DeletionAction::Applied { kind: "traces" }.into(),
        DeletionAction::Failed { kind: "traces" }.into(),
        EvaluatorAction::Created {
            kind: "webhook",
            url_host: Some("hooks.example".to_owned()),
        }
        .into(),
        EvaluatorAction::Updated {
            fields: vec!["config"],
            url_host: None,
        }
        .into(),
        EvaluatorAction::Deleted.into(),
        OnlineRuleAction::Created { evaluator_id: uuid }.into(),
        OnlineRuleAction::Updated {
            fields: vec!["sample_rate"],
            budget_resumed: true,
        }
        .into(),
        OnlineRuleAction::Deleted.into(),
        OnlineRuleAction::Paused {
            reason: "budget_exhausted",
        }
        .into(),
        OnlineRuleAction::Resumed { reason: "manual" }.into(),
        OnlineRuleAction::BackfillStarted {
            backfill_id: uuid,
            max_targets: 500,
        }
        .into(),
        ScoreDefinitionAction::Created {
            data_type: "numeric",
        }
        .into(),
        ScoreDefinitionAction::Updated {
            fields: vec!["description"],
        }
        .into(),
        ScoreDefinitionAction::Archived.into(),
        AlertRuleAction::Created {
            metric: "error_rate",
            enabled: true,
        }
        .into(),
        AlertRuleAction::Updated {
            fields: vec!["threshold"],
        }
        .into(),
        AlertRuleAction::Deleted.into(),
        AlertChannelAction::Created { kind: "webhook" }.into(),
        AlertChannelAction::Updated {
            fields: vec!["enabled"],
        }
        .into(),
        AlertChannelAction::Deleted.into(),
        LlmConnectionAction::Created { adapter: "openai" }.into(),
        LlmConnectionAction::Updated {
            fields: vec!["base_url"],
            key_changed: true,
        }
        .into(),
        LlmConnectionAction::Deleted.into(),
        ModelPriceAction::Created.into(),
        ModelPriceAction::Updated.into(),
        ModelPriceAction::Deleted.into(),
        AssistantAction::SettingChanged {
            enabled: true,
            connection_id: Some(uuid),
        }
        .into(),
        PromptAction::Created.into(),
        PromptAction::Updated {
            fields: vec!["description"],
        }
        .into(),
        PromptAction::Deleted.into(),
        PromptAction::VersionCreated { version: 3 }.into(),
        PromptAction::LabelSet {
            label: "production".to_owned(),
            version: 3,
        }
        .into(),
        PromptAction::LabelRemoved {
            label: "staging".to_owned(),
        }
        .into(),
        PromptAction::ProtectedLabelsChanged { count: 2 }.into(),
        DatasetAction::Created.into(),
        DatasetAction::Updated {
            fields: vec!["description"],
        }
        .into(),
        DatasetAction::Deleted.into(),
        DatasetAction::ExportStarted {
            version: 2,
            format: "jsonl",
        }
        .into(),
        IssueSourceAction::Created { kind: "github" }.into(),
        IssueSourceAction::Updated {
            fields: vec!["repo"],
        }
        .into(),
        IssueSourceAction::Deleted.into(),
        DashboardAction::Created.into(),
        DashboardAction::Updated.into(),
        DashboardAction::Deleted.into(),
        DashboardAction::OverviewSaved { created: true }.into(),
        DashboardAction::OverviewReset.into(),
        AnnotationQueueAction::Created.into(),
        AnnotationQueueAction::Updated.into(),
        AnnotationQueueAction::Deleted.into(),
        SqlAction::Run {
            query_sha256: "0".repeat(64),
        }
        .into(),
        SqlAction::ExportStarted {
            query_sha256: "0".repeat(64),
            format: "csv",
        }
        .into(),
        McpAction::ToolCalled {
            tool: "query_sql",
            query_sha256: Some("0".repeat(64)),
        }
        .into(),
        AuditAction::ExportStarted {
            format: "csv",
            filters: vec!["from", "action"],
        }
        .into(),
    ]
}

/// The group each variant belongs to, as its name prefix.
fn group(action: &Action) -> &'static str {
    match action {
        Action::Auth(_) => "auth",
        Action::Org(_) => "org",
        Action::Project(_) => "project",
        Action::Member(_) => "member",
        Action::Invite(_) => "invite",
        Action::ApiKey(_) => "api_key",
        Action::Pat(_) => "pat",
        Action::Sso(_) => "sso",
        Action::Scim(_) => "scim",
        Action::User(_) => "user",
        Action::Instance(_) => "instance",
        Action::Deletion(_) => "deletion",
        Action::Evaluator(_) => "evaluator",
        Action::OnlineRule(_) => "online_rule",
        Action::ScoreDefinition(_) => "score_definition",
        Action::AlertRule(_) => "alert_rule",
        Action::AlertChannel(_) => "alert_channel",
        Action::LlmConnection(_) => "llm_connection",
        Action::ModelPrice(_) => "model_price",
        Action::Assistant(_) => "assistant",
        Action::Prompt(_) => "prompt",
        Action::Dataset(_) => "dataset",
        Action::IssueSource(_) => "issue_source",
        Action::Dashboard(_) => "dashboard",
        Action::AnnotationQueue(_) => "annotation_queue",
        Action::Sql(_) => "sql",
        Action::Mcp(_) => "mcp",
        Action::Audit(_) => "audit",
    }
}

/// The `action` name and the `details` object of a sample, as the serialized value holds them.
fn name_and_details(action: &Action) -> (String, serde_json::Value) {
    let value = serde_json::to_value(action).expect("the catalog serializes");
    let name = value["action"].as_str().expect("a name").to_owned();
    let details = value
        .get("details")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    (name, details)
}

async fn row_count(meta: &MetaDb) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_events")
        .fetch_one(meta.reader())
        .await
        .expect("count")
}

#[tokio::test]
async fn record_writes_one_row_with_every_column() {
    let (_dir, meta) = crate::meta::test_db().await;
    let user = Uuid::now_v7();
    let pat = Uuid::now_v7();
    let org = OrgId::new_v7();
    let project = ProjectId::new_v7();
    let target = Uuid::now_v7();
    let ctx = Ctx {
        actor: Actor::User {
            id: user,
            via: Via::Pat,
            credential_id: Some(pat),
        },
        ip: Some("203.0.113.7".parse().expect("ip")),
        user_agent: Some("agent/1".to_owned()),
    };
    let event = Event::new(MemberAction::RoleChanged {
        old_role: "viewer",
        new_role: "admin",
    })
    .org(org)
    .project(project)
    .target(TargetKind::User, target);
    meta.write(move |conn| Box::pin(async move { record(conn, &ctx, event, 42).await }))
        .await
        .expect("the record is written");

    let row = sqlx::query(
        "SELECT seq, id, at, org_id, project_id, action, outcome, actor_kind, actor_id, via, \
         credential_id, target_kind, target_id, details, ip, user_agent FROM audit_events",
    )
    .fetch_one(meta.reader())
    .await
    .expect("one row");
    assert_eq!(row.get::<i64, _>("seq"), 1);
    let id: String = row.get("id");
    assert_eq!(Uuid::parse_str(&id).expect("uuid").get_version_num(), 7);
    assert_eq!(row.get::<i64, _>("at"), 42);
    assert_eq!(
        row.get::<Option<String>, _>("org_id"),
        Some(org.to_string())
    );
    assert_eq!(
        row.get::<Option<String>, _>("project_id"),
        Some(project.to_string())
    );
    assert_eq!(row.get::<String, _>("action"), "member.role_changed");
    assert_eq!(row.get::<String, _>("outcome"), "success");
    assert_eq!(row.get::<String, _>("actor_kind"), "user");
    assert_eq!(
        row.get::<Option<String>, _>("actor_id"),
        Some(user.to_string())
    );
    assert_eq!(row.get::<Option<String>, _>("via"), Some("pat".to_owned()));
    assert_eq!(
        row.get::<Option<String>, _>("credential_id"),
        Some(pat.to_string())
    );
    assert_eq!(
        row.get::<Option<String>, _>("target_kind"),
        Some("user".to_owned())
    );
    assert_eq!(
        row.get::<Option<String>, _>("target_id"),
        Some(target.to_string())
    );
    let details: serde_json::Value =
        serde_json::from_str(&row.get::<String, _>("details")).expect("details is JSON");
    assert_eq!(
        details,
        serde_json::json!({"old_role": "viewer", "new_role": "admin"})
    );
    assert_eq!(
        row.get::<Option<String>, _>("ip"),
        Some("203.0.113.7".to_owned())
    );
    assert_eq!(
        row.get::<Option<String>, _>("user_agent"),
        Some("agent/1".to_owned())
    );
}

#[tokio::test]
async fn a_unit_variant_stores_empty_details() {
    let (_dir, meta) = crate::meta::test_db().await;
    record_alone(&meta, &Ctx::system(), Event::new(AuthAction::Logout), 1)
        .await
        .expect("written");
    let details: String = sqlx::query_scalar("SELECT details FROM audit_events")
        .fetch_one(meta.reader())
        .await
        .expect("one row");
    assert_eq!(details, "{}");
}

#[tokio::test]
async fn record_in_a_rolled_back_transaction_leaves_no_row() {
    let (_dir, meta) = crate::meta::test_db().await;
    let ctx = Ctx::system();
    let event = Event::new(AuthAction::Logout);
    let result: Result<(), MetaError> = meta
        .write(move |conn| {
            Box::pin(async move {
                record(conn, &ctx, event, 1).await?;
                Err::<(), MetaError>(MetaError::Conflict("x"))
            })
        })
        .await;
    assert!(matches!(result, Err(MetaError::Conflict("x"))));
    assert_eq!(row_count(&meta).await, 0);
}

#[tokio::test]
async fn record_alone_commits() {
    let (_dir, meta) = crate::meta::test_db().await;
    record_alone(&meta, &Ctx::system(), Event::new(AuthAction::Logout), 1)
        .await
        .expect("written");
    assert_eq!(row_count(&meta).await, 1);
}

#[tokio::test]
async fn seq_follows_commit_order() {
    let (_dir, meta) = crate::meta::test_db().await;
    for at in 1..=3 {
        record_alone(&meta, &Ctx::system(), Event::new(AuthAction::Logout), at)
            .await
            .expect("written");
    }
    let seqs: Vec<i64> = sqlx::query_scalar("SELECT seq FROM audit_events ORDER BY seq")
        .fetch_all(meta.reader())
        .await
        .expect("seqs");
    assert_eq!(seqs, vec![1, 2, 3]);
}

#[test]
fn every_action_name_is_group_dot_snake() {
    for action in samples() {
        let (name, _) = name_and_details(&action);
        let (prefix, rest) = name
            .split_once('.')
            .unwrap_or_else(|| panic!("{name} has no dot"));
        assert!(
            !rest.contains('.')
                && !prefix.is_empty()
                && !rest.is_empty()
                && prefix
                    .chars()
                    .chain(rest.chars())
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
            "{name} is not group.snake_name"
        );
        assert_eq!(prefix, group(&action), "{name} is in the wrong group");
    }
}

#[test]
fn a_scim_token_is_the_actor_and_the_target_kind() {
    let token = Uuid::now_v7();
    let actor = Actor::Scim { token_id: token };
    assert_eq!(actor.kind(), "scim_token");
    assert_eq!(actor.id(), Some(token.to_string()));
    assert_eq!(actor.via(), None);
    assert_eq!(actor.credential_id(), None);
    assert_eq!(TargetKind::ScimToken.as_str(), "scim_token");
    assert_eq!(TargetKind::ScimGroup.as_str(), "scim_group");
}

#[test]
fn action_names_are_unique() {
    let mut seen = HashSet::new();
    for action in samples() {
        let (name, _) = name_and_details(&action);
        assert!(seen.insert(name.clone()), "{name} is used twice");
    }
}

/// Keys that would carry an email, a secret, a name, a URL, SQL, a prompt, a subject or a domain.
const FORBIDDEN_KEYS: [&str; 10] = [
    "email", "password", "secret", "token", "name", "url", "sql", "prompt", "subject", "domain",
];

fn assert_no_forbidden_key(name: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                assert!(
                    !FORBIDDEN_KEYS
                        .iter()
                        .any(|forbidden| key == *forbidden || key.ends_with(*forbidden)),
                    "{name} records the forbidden key {key}"
                );
                assert_no_forbidden_key(name, child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_no_forbidden_key(name, item);
            }
        }
        _ => {}
    }
}

#[test]
fn details_hold_no_forbidden_field() {
    for action in samples() {
        let (name, details) = name_and_details(&action);
        assert_no_forbidden_key(&name, &details);
    }
}

#[tokio::test]
async fn login_failed_is_recorded_once_a_minute() {
    let (_dir, meta) = crate::meta::test_db().await;
    let user = Uuid::now_v7();
    let ctx = Ctx::system();
    let t0 = 1_000 * SECOND;
    record_login_failed(&meta, &ctx, user, t0)
        .await
        .expect("first failure");
    record_login_failed(&meta, &ctx, user, t0 + 10 * SECOND)
        .await
        .expect("second failure");
    assert_eq!(row_count(&meta).await, 1);
    assert_eq!(LOGIN_FAILED_WINDOW_NS, 60 * SECOND);
    record_login_failed(&meta, &ctx, user, t0 + 61 * SECOND)
        .await
        .expect("third failure");
    assert_eq!(row_count(&meta).await, 2);
}

fn request_parts(user_agent: Option<&str>) -> axum::http::request::Parts {
    let mut builder = axum::http::Request::builder().uri("/");
    if let Some(value) = user_agent {
        builder = builder.header(USER_AGENT, value);
    }
    builder.body(()).expect("request").into_parts().0
}

fn session_user(id: Uuid) -> AuthUser {
    AuthUser {
        id,
        is_instance_admin: false,
        email: "user@example.com".to_owned(),
    }
}

#[tokio::test]
async fn ctx_reads_the_principal_ip_and_user_agent() {
    let app = TestApp::with_config(|config| config.trusted_proxies.clear()).await;
    let state: &AppState = &app.state;
    let user = Uuid::now_v7();

    // A session principal, a 300-character user agent, and a peer.
    let mut parts = request_parts(Some(&"a".repeat(300)));
    parts.extensions.insert(Principal::Session {
        user: session_user(user),
        session_hash: "hash".to_owned(),
        sso_org: None,
    });
    parts
        .extensions
        .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 4_000))));
    let ctx = Ctx::from_request_parts(&mut parts, state)
        .await
        .expect("infallible");
    assert_eq!(
        ctx.actor,
        Actor::User {
            id: user,
            via: Via::Session,
            credential_id: None,
        }
    );
    assert_eq!(ctx.ip, Some("10.0.0.1".parse().expect("ip")));
    assert_eq!(
        ctx.user_agent.as_deref().map(|value| value.chars().count()),
        Some(256)
    );

    // An `Option<Principal>` extension, as `authenticate` inserts it.
    let mut parts = request_parts(None);
    parts.extensions.insert(Some(Principal::Session {
        user: session_user(user),
        session_hash: "hash".to_owned(),
        sso_org: None,
    }));
    let ctx = Ctx::from_request_parts(&mut parts, state)
        .await
        .expect("infallible");
    assert_eq!(ctx.actor.kind(), "user");
    assert_eq!(ctx.ip, None);
    assert_eq!(ctx.user_agent, None);

    // No ConnectInfo: no IP.
    let mut parts = request_parts(Some("agent/2"));
    let ctx = Ctx::from_request_parts(&mut parts, state)
        .await
        .expect("infallible");
    assert_eq!(ctx.ip, None);

    // No principal: anonymous.
    assert_eq!(ctx.actor, Actor::Anonymous);
}

#[test]
fn actor_of_maps_every_principal() {
    let user = Uuid::now_v7();
    let token = Uuid::now_v7();
    let key = Uuid::now_v7();

    let session = Principal::Session {
        user: session_user(user),
        session_hash: "hash".to_owned(),
        sso_org: None,
    };
    assert_eq!(
        Actor::of(&session),
        Actor::User {
            id: user,
            via: Via::Session,
            credential_id: None,
        }
    );

    let pat = Principal::Pat {
        user: session_user(user),
        token_id: token,
        write: false,
    };
    assert_eq!(
        Actor::of(&pat),
        Actor::User {
            id: user,
            via: Via::Pat,
            credential_id: Some(token),
        }
    );

    let api_key = Principal::ApiKey {
        key_id: key,
        project_id: ProjectId::new_v7(),
        kind: KeyKind::Secret,
        scopes: KeyScopes::from_scopes(&[KeyScope::Read]),
    };
    assert_eq!(Actor::of(&api_key), Actor::ApiKey { key_id: key });

    let control_plane = Principal::ControlPlane {
        token_id: token,
        scope: "org".to_owned(),
        method: "PUT".to_owned(),
        path: "/x".to_owned(),
    };
    assert_eq!(
        Actor::of(&control_plane),
        Actor::ControlPlane {
            token_id: token.to_string(),
        }
    );
}

#[tokio::test]
async fn the_tracing_line_names_the_action_and_no_ip() {
    let user = Uuid::now_v7();
    let ctx = Ctx {
        actor: Actor::User {
            id: user,
            via: Via::Session,
            credential_id: None,
        },
        ip: Some("198.51.100.23".parse().expect("ip")),
        user_agent: Some("marker-agent".to_owned()),
    };
    let event = Event::new(MemberAction::RoleChanged {
        old_role: "viewer",
        new_role: "admin",
    })
    .target(TargetKind::User, user);
    let (result, logs) = capture_logs(async move {
        let (_dir, meta) = crate::meta::test_db().await;
        meta.write(move |conn| Box::pin(async move { record(conn, &ctx, event, 1).await }))
            .await
    })
    .await;
    result.expect("written");
    assert!(logs.contains("audit"), "{logs}");
    assert!(logs.contains("member.role_changed"), "{logs}");
    assert!(logs.contains(&user.to_string()), "{logs}");
    assert!(!logs.contains("198.51.100.23"), "{logs}");
    assert!(!logs.contains("marker-agent"), "{logs}");
}

#[test]
fn sha256_hex_matches_a_known_vector() {
    assert_eq!(
        sha256_hex("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

fn connection_fields() -> ConnectionFields {
    ConnectionFields {
        name: "main".to_owned(),
        adapter: "openai".to_owned(),
        base_url: None,
        secret_enc: vec![1],
        api_key_hint: "WXYZ".to_owned(),
        extra_headers_enc: None,
        extra_header_names: Vec::new(),
        default_model: None,
        structured_output: "native".to_owned(),
        max_concurrency: 4,
        requests_per_minute: 300,
    }
}

fn evaluator_fields(connection: Option<Uuid>) -> EvaluatorFields {
    EvaluatorFields {
        name: "relevance".to_owned(),
        config: r#"{"model":"gpt-5-mini"}"#.to_owned(),
        output_score: r#"{"name":"relevance","data_type":"numeric"}"#.to_owned(),
        connection_id: connection,
        secret_enc: None,
        version: 1,
    }
}

fn rule_fields() -> RuleFields {
    RuleFields {
        name: "relevance rule".to_owned(),
        filter: None,
        variable_mapping: r#"{"input":"trace.input"}"#.to_owned(),
        sample_rate: 1.0,
        budget_usd: None,
        budget_period: None,
        enabled: true,
    }
}

/// An org, a project and an online rule on a judge evaluator. Returns the org, the project
/// and the rule ids.
async fn seed_rule(meta: &MetaDb) -> (OrgId, ProjectId, Uuid) {
    meta.write(|conn| {
        Box::pin(async move {
            let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", 1)
                .await?
                .id;
            let project = insert_project(conn, org, "Support", "support", None, 1)
                .await?
                .id;
            let connection = Uuid::now_v7();
            insert_connection(conn, connection, org, connection_fields(), None, 1).await?;
            let evaluator = Uuid::now_v7();
            insert_evaluator(
                conn,
                evaluator,
                project,
                "llm_judge",
                evaluator_fields(Some(connection)),
                None,
                1,
            )
            .await?;
            let rule = Uuid::now_v7();
            insert_rule(
                conn,
                NewRule {
                    id: rule,
                    project_id: project,
                    evaluator_id: evaluator,
                    target: "trace".to_owned(),
                    created_by: None,
                },
                rule_fields(),
                1,
            )
            .await?;
            Ok((org, project, rule))
        })
    })
    .await
    .expect("seeded")
}

#[tokio::test]
async fn scope_of_rule_finds_the_project_and_org() {
    let (_dir, meta) = crate::meta::test_db().await;
    let (org, project, rule) = seed_rule(&meta).await;
    let scope = meta
        .write(move |conn| Box::pin(async move { scope_of_rule(conn, rule).await }))
        .await
        .expect("read");
    assert_eq!(scope, Some((org, project)));
}

#[tokio::test]
async fn scope_of_rule_of_an_unknown_rule_is_none() {
    let (_dir, meta) = crate::meta::test_db().await;
    seed_rule(&meta).await;
    let unknown = Uuid::now_v7();
    let scope = meta
        .write(move |conn| Box::pin(async move { scope_of_rule(conn, unknown).await }))
        .await
        .expect("read");
    assert_eq!(scope, None);
}
