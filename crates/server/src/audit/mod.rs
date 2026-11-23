//! Audit records (track EA, D-EA-1 to D-EA-9 and D-EA-20).
//!
//! Each action of the catalog writes one row to `audit_events` in the transaction of its
//! change, or in a transaction of its own when it changes nothing in `meta.db`. [`record`]
//! is the only writer, and it also emits the `audit` tracing line.

pub(crate) mod catalog;
pub(crate) mod retention;
#[cfg(test)]
mod tests;

pub(crate) use catalog::Action;
// The lowercase hex SHA-256 of a text, as `query_sha256` and the other hashes in details.
pub(crate) use crate::auth::secret::hash_token as sha256_hex;

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::header::USER_AGENT;
use axum::http::request::Parts;
use cetra_core::ids::{OrgId, ProjectId};

use self::catalog::AuthAction;
use crate::auth::principal::Principal;
use crate::http::AppState;
use crate::http::client_ip::client_ip;
use crate::meta::{MetaDb, MetaError};

/// User agents are cut to this many characters (as `web_sessions`).
pub(crate) const USER_AGENT_MAX: usize = 256;
/// A failed sign-in of one account is recorded at most once per this window (D-EA-2).
pub(crate) const LOGIN_FAILED_WINDOW_NS: i64 = 60 * 1_000_000_000;

/// How a user acted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    Session,
    Pat,
}

/// Who acted (D-EA-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Actor {
    /// A signed-in user. `credential_id` is the PAT id when `via` is `Pat`.
    User {
        id: uuid::Uuid,
        via: Via,
        credential_id: Option<uuid::Uuid>,
    },
    /// A project API key.
    ApiKey { key_id: uuid::Uuid },
    /// The control plane, by its token id.
    ControlPlane { token_id: String },
    /// `cetra admin` on the server's host.
    Cli,
    /// The server itself: workers, startup, retention.
    System,
    /// No credential.
    Anonymous,
}

impl Actor {
    /// The `actor_kind` column.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::ApiKey { .. } => "api_key",
            Self::ControlPlane { .. } => "control_plane",
            Self::Cli => "cli",
            Self::System => "system",
            Self::Anonymous => "anonymous",
        }
    }

    /// The `actor_id` column: the user id, key id or token id; `None` otherwise.
    pub(crate) fn id(&self) -> Option<String> {
        match self {
            Self::User { id, .. } => Some(id.to_string()),
            Self::ApiKey { key_id } => Some(key_id.to_string()),
            Self::ControlPlane { token_id } => Some(token_id.clone()),
            Self::Cli | Self::System | Self::Anonymous => None,
        }
    }

    /// The `via` column: `session` or `pat` for a user; `None` otherwise.
    pub(crate) fn via(&self) -> Option<&'static str> {
        match self {
            Self::User {
                via: Via::Session, ..
            } => Some("session"),
            Self::User { via: Via::Pat, .. } => Some("pat"),
            _ => None,
        }
    }

    /// The `credential_id` column: the PAT id for a user acting through a PAT.
    pub(crate) fn credential_id(&self) -> Option<String> {
        match self {
            Self::User { credential_id, .. } => credential_id.map(|id| id.to_string()),
            _ => None,
        }
    }

    /// The actor a principal stands for. `Session` → `User{via: Session}`,
    /// `Pat` → `User{via: Pat, credential_id: Some(token_id)}`, `ApiKey` → `ApiKey`,
    /// `ControlPlane` → `ControlPlane{token_id}`.
    pub(crate) fn of(principal: &Principal) -> Self {
        match principal {
            Principal::Session { user, .. } => Self::User {
                id: user.id,
                via: Via::Session,
                credential_id: None,
            },
            Principal::Pat { user, token_id, .. } => Self::User {
                id: user.id,
                via: Via::Pat,
                credential_id: Some(*token_id),
            },
            Principal::ApiKey { key_id, .. } => Self::ApiKey { key_id: *key_id },
            Principal::ControlPlane { token_id, .. } => Self::ControlPlane {
                token_id: token_id.to_string(),
            },
        }
    }
}

/// The request side of a record (D-EA-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ctx {
    pub(crate) actor: Actor,
    pub(crate) ip: Option<IpAddr>,
    pub(crate) user_agent: Option<String>,
}

impl Ctx {
    /// No request: workers, startup and retention.
    pub(crate) fn system() -> Self {
        Self {
            actor: Actor::System,
            ip: None,
            user_agent: None,
        }
    }

    /// `cetra admin` commands.
    pub(crate) fn cli() -> Self {
        Self {
            actor: Actor::Cli,
            ip: None,
            user_agent: None,
        }
    }

    /// The same request with another actor: a sign-in names the user it signed in.
    pub(crate) fn with_actor(&self, actor: Actor) -> Self {
        Self {
            actor,
            ..self.clone()
        }
    }

    /// A user acting through a session, from the given request values; `user_agent`
    /// is cut with [`truncate_user_agent`]. Used by `start_session`.
    pub(crate) fn for_session_user(
        id: uuid::Uuid,
        ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Self {
        Self {
            actor: Actor::User {
                id,
                via: Via::Session,
                credential_id: None,
            },
            ip,
            user_agent: user_agent.map(truncate_user_agent),
        }
    }
}

/// Reads the actor, the client IP and the user agent of a request. It never fails.
impl<S> FromRequestParts<S> for Ctx
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let state = AppState::from_ref(state);
        let actor = parts
            .extensions
            .get::<Principal>()
            .or_else(|| {
                parts
                    .extensions
                    .get::<Option<Principal>>()
                    .and_then(Option::as_ref)
            })
            .map_or(Actor::Anonymous, Actor::of);
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| {
                client_ip(peer.ip(), &parts.headers, &state.config.trusted_proxies)
            });
        let user_agent = parts
            .headers
            .get(USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(truncate_user_agent);
        Ok(Self {
            actor,
            ip,
            user_agent,
        })
    }
}

/// Cuts a user agent to [`USER_AGENT_MAX`] characters, on a char boundary.
pub(crate) fn truncate_user_agent(value: &str) -> String {
    value.chars().take(USER_AGENT_MAX).collect()
}

/// `success` or `failure` (D-EA-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Success,
    Failure,
}

impl Outcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

/// The `target_kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    User,
    Invite,
    ApiKey,
    Pat,
    Project,
    Org,
    SsoProvider,
    SsoDomain,
    Deletion,
    License,
    Evaluator,
    OnlineRule,
    ScoreDefinition,
    AlertRule,
    AlertChannel,
    LlmConnection,
    ModelPrice,
    Prompt,
    Dataset,
    IssueSource,
    Dashboard,
    AnnotationQueue,
}

impl TargetKind {
    /// user | invite | api_key | pat | project | org | sso_provider | sso_domain | deletion | license
    /// | evaluator | online_rule | score_definition | alert_rule | alert_channel | llm_connection
    /// | model_price | prompt | dataset | issue_source | dashboard | annotation_queue
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Invite => "invite",
            Self::ApiKey => "api_key",
            Self::Pat => "pat",
            Self::Project => "project",
            Self::Org => "org",
            Self::SsoProvider => "sso_provider",
            Self::SsoDomain => "sso_domain",
            Self::Deletion => "deletion",
            Self::License => "license",
            Self::Evaluator => "evaluator",
            Self::OnlineRule => "online_rule",
            Self::ScoreDefinition => "score_definition",
            Self::AlertRule => "alert_rule",
            Self::AlertChannel => "alert_channel",
            Self::LlmConnection => "llm_connection",
            Self::ModelPrice => "model_price",
            Self::Prompt => "prompt",
            Self::Dataset => "dataset",
            Self::IssueSource => "issue_source",
            Self::Dashboard => "dashboard",
            Self::AnnotationQueue => "annotation_queue",
        }
    }
}

/// One record before it is written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Event {
    pub(crate) org_id: Option<OrgId>,
    pub(crate) project_id: Option<ProjectId>,
    pub(crate) target: Option<(TargetKind, String)>,
    pub(crate) action: Action,
    pub(crate) outcome: Outcome,
}

impl Event {
    /// An instance record (no org), success, no target.
    pub(crate) fn new(action: impl Into<Action>) -> Self {
        Self {
            org_id: None,
            project_id: None,
            target: None,
            action: action.into(),
            outcome: Outcome::Success,
        }
    }

    pub(crate) fn org(mut self, org: OrgId) -> Self {
        self.org_id = Some(org);
        self
    }

    pub(crate) fn project(mut self, project: ProjectId) -> Self {
        self.project_id = Some(project);
        self
    }

    // Call sites pass owned ids (`Uuid`, `OrgId`, …); taking them by value reads best there.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn target(mut self, kind: TargetKind, id: impl ToString) -> Self {
        self.target = Some((kind, id.to_string()));
        self
    }

    pub(crate) fn failed(mut self) -> Self {
        self.outcome = Outcome::Failure;
        self
    }
}

/// The org and project of an online rule, for worker records. `None` when the rule does not exist.
pub(crate) async fn scope_of_rule(
    conn: &mut sqlx::SqliteConnection,
    rule: uuid::Uuid,
) -> Result<Option<(OrgId, ProjectId)>, MetaError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT p.org_id, p.id FROM online_rules r JOIN projects p ON p.id = r.project_id \
         WHERE r.id = ?",
    )
    .bind(rule.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(MetaError::from)?;
    let Some((org, project)) = row else {
        return Ok(None);
    };
    let org = uuid::Uuid::parse_str(&org).map_err(|_| MetaError::Corrupt {
        table: "projects",
        column: "org_id",
    })?;
    let project = uuid::Uuid::parse_str(&project).map_err(|_| MetaError::Corrupt {
        table: "projects",
        column: "id",
    })?;
    Ok(Some((OrgId::from_uuid(org), ProjectId::from_uuid(project))))
}

fn corrupt_details() -> MetaError {
    MetaError::Corrupt {
        table: "audit_events",
        column: "details",
    }
}

fn or_dash(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

/// Writes `event` in the caller's transaction, then emits the `audit` tracing line.
/// Call it last before the closure's success `Ok`, after every check that can roll
/// back (plan P-1).
///
/// # Errors
/// The insert's [`MetaError`]; the caller's transaction then rolls back (D-EA-2).
pub(crate) async fn record(
    conn: &mut sqlx::SqliteConnection,
    ctx: &Ctx,
    event: Event,
    now: i64,
) -> Result<(), MetaError> {
    let value = serde_json::to_value(&event.action).map_err(|_| corrupt_details())?;
    let name = value
        .get("action")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(corrupt_details)?
        .to_owned();
    let details = value
        .get("details")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let details_json = serde_json::to_string(&details).map_err(|_| corrupt_details())?;

    let id = uuid::Uuid::now_v7().to_string();
    let org_id = event.org_id.map(|org| org.to_string());
    let project_id = event.project_id.map(|project| project.to_string());
    let (target_kind, target_id) = match &event.target {
        Some((kind, id)) => (Some(kind.as_str()), Some(id.as_str())),
        None => (None, None),
    };
    let actor_id = ctx.actor.id();
    let ip = ctx.ip.map(|ip| ip.to_string());

    sqlx::query(
        "INSERT INTO audit_events (id, at, org_id, project_id, action, outcome, actor_kind, \
         actor_id, via, credential_id, target_kind, target_id, details, ip, user_agent) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.as_str())
    .bind(now)
    .bind(org_id.as_deref())
    .bind(project_id.as_deref())
    .bind(name.as_str())
    .bind(event.outcome.as_str())
    .bind(ctx.actor.kind())
    .bind(actor_id.as_deref())
    .bind(ctx.actor.via())
    .bind(ctx.actor.credential_id())
    .bind(target_kind)
    .bind(target_id)
    .bind(details_json.as_str())
    .bind(ip.as_deref())
    .bind(ctx.user_agent.as_deref())
    .execute(&mut *conn)
    .await
    .map_err(MetaError::from)?;

    tracing::info!(
        target: "audit",
        action = name.as_str(),
        org_id = or_dash(org_id.as_deref()),
        project_id = or_dash(project_id.as_deref()),
        actor_kind = ctx.actor.kind(),
        actor_id = or_dash(actor_id.as_deref()),
        target_kind = or_dash(target_kind),
        target_id = or_dash(target_id),
        outcome = event.outcome.as_str(),
        details = %details_json,
        "{name}"
    );
    Ok(())
}

/// [`record`] in a transaction of its own, for actions that change nothing in
/// `meta.db` (sign-in failures, device denials, registrations).
pub(crate) async fn record_alone(
    meta: &MetaDb,
    ctx: &Ctx,
    event: Event,
    now: i64,
) -> Result<(), MetaError> {
    let ctx = ctx.clone();
    meta.write(move |conn| Box::pin(async move { record(conn, &ctx, event, now).await }))
        .await
}

/// `auth.login_failed` for a known account, skipped when one was recorded for
/// `user_id` in the last [`LOGIN_FAILED_WINDOW_NS`] (plan P-4). Own transaction.
pub(crate) async fn record_login_failed(
    meta: &MetaDb,
    ctx: &Ctx,
    user_id: uuid::Uuid,
    now: i64,
) -> Result<(), MetaError> {
    let ctx = ctx.clone();
    let since = now - LOGIN_FAILED_WINDOW_NS;
    meta.write(move |conn| {
        Box::pin(async move {
            let recent = sqlx::query_scalar::<_, i32>(
                "SELECT 1 FROM audit_events WHERE org_id IS NULL AND target_id = ? \
                 AND action = 'auth.login_failed' AND at >= ? LIMIT 1",
            )
            .bind(user_id.to_string())
            .bind(since)
            .fetch_optional(&mut *conn)
            .await
            .map_err(MetaError::from)?;
            if recent.is_some() {
                return Ok(());
            }
            let event = Event::new(AuthAction::LoginFailed)
                .target(TargetKind::User, user_id)
                .failed();
            record(conn, &ctx, event, now).await
        })
    })
    .await
}
