// SPDX-License-Identifier: Elastic-2.0
//! Reads of the audit log (track EA M3): filters, `seq` paging and the display names resolved
//! per page (D-EA-3, D-EA-8, D-EA-20; plan P-1 to P-3).

use std::collections::{BTreeSet, HashMap, HashSet};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cetra_core::ids::{OrgId, ProjectId};
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};
use uuid::Uuid;

use crate::api::audit::{AuditActor, AuditEvent, AuditOutcome, AuditRef, AuditRefStatus};
use crate::api::common::TimeParam;
use crate::http::error::{ApiError, field_error};
use crate::meta::MetaError;

/// Most action entries one `action` filter takes (plan P-2).
const MAX_ACTIONS: usize = 20;
/// Most characters of an `actor_id` or `target_id` filter (plan P-2).
const MAX_ID_CHARS: usize = 64;

/// Which log is read: one org's records, or the instance's (`org_id` is null).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    Org(OrgId),
    Instance,
}

/// The validated filters of a read. `from` is inclusive and `to` exclusive, in nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Filters {
    pub(crate) from: Option<i64>,
    pub(crate) to: Option<i64>,
    pub(crate) actions: Vec<ActionMatch>,
    pub(crate) actor_id: Option<String>,
    pub(crate) target_id: Option<String>,
    pub(crate) project_id: Option<ProjectId>,
    pub(crate) outcome: Option<&'static str>,
}

/// One entry of the `action` filter: an exact name, or a group prefix that ends with `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionMatch {
    Exact(String),
    /// The entry as written, dot included: `member.`.
    Prefix(String),
}

/// One stored record, as read. `details` is the stored JSON text.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Row {
    pub(crate) seq: i64,
    pub(crate) id: String,
    pub(crate) at: i64,
    pub(crate) project_id: Option<String>,
    pub(crate) action: String,
    pub(crate) outcome: String,
    pub(crate) actor_kind: String,
    pub(crate) actor_id: Option<String>,
    pub(crate) via: Option<String>,
    pub(crate) credential_id: Option<String>,
    pub(crate) target_kind: Option<String>,
    pub(crate) target_id: Option<String>,
    pub(crate) details: String,
    pub(crate) ip: Option<String>,
    pub(crate) user_agent: Option<String>,
}

/// Validates the list and export filters (plan P-2). Errors are `field_error`s.
pub(crate) fn filters(
    from: Option<&TimeParam>,
    to: Option<&TimeParam>,
    action: Option<&str>,
    actor_id: Option<&str>,
    target_id: Option<&str>,
    project_id: Option<Uuid>,
    outcome: Option<AuditOutcome>,
) -> Result<Filters, ApiError> {
    Ok(Filters {
        from: from.map(|time| time.ns()),
        to: to.map(|time| time.ns()),
        actions: action.map(parse_actions).transpose()?.unwrap_or_default(),
        actor_id: id_filter("actor_id", actor_id)?,
        target_id: id_filter("target_id", target_id)?,
        project_id: project_id.map(ProjectId::from_uuid),
        outcome: outcome.map(outcome_name),
    })
}

fn outcome_name(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Success => "success",
        AuditOutcome::Failure => "failure",
    }
}

fn id_filter(field: &'static str, value: Option<&str>) -> Result<Option<String>, ApiError> {
    match value {
        Some(value) if value.chars().count() > MAX_ID_CHARS => {
            Err(field_error(field, "must be at most 64 characters"))
        }
        Some(value) => Ok(Some(value.to_owned())),
        None => Ok(None),
    }
}

/// Splits a comma-separated `action` filter (plan P-2).
fn parse_actions(text: &str) -> Result<Vec<ActionMatch>, ApiError> {
    let entries: Vec<&str> = text.split(',').collect();
    if entries.len() > MAX_ACTIONS {
        return Err(field_error("action", "at most 20 actions"));
    }
    entries
        .into_iter()
        .map(|entry| {
            if !valid_entry(entry) {
                return Err(field_error(
                    "action",
                    "each entry must be an action name such as auth.login, or a group such as member.",
                ));
            }
            Ok(if entry.ends_with('.') {
                ActionMatch::Prefix(entry.to_owned())
            } else {
                ActionMatch::Exact(entry.to_owned())
            })
        })
        .collect()
}

/// `^[a-z_]+(\.[a-z_]+)?\.?$`: one or two dotted segments, the last dot optional.
fn valid_entry(entry: &str) -> bool {
    let body = entry.strip_suffix('.').unwrap_or(entry);
    let parts: Vec<&str> = body.split('.').collect();
    (1..=2).contains(&parts.len()) && parts.iter().all(|part| is_segment(part))
}

fn is_segment(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// `text` with the LIKE wildcards and the escape character itself escaped (`ESCAPE '\'`).
/// A copy of `meta::datasets`'s helper, which is private there.
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// One page of rows, newest first: `seq < before_seq` when given, at most `limit` rows (the
/// caller asks for one more than it shows, to learn whether a next page exists).
pub(crate) async fn page(
    pool: &SqlitePool,
    scope: &Scope,
    filters: &Filters,
    before_seq: Option<i64>,
    limit: u32,
) -> Result<Vec<Row>, MetaError> {
    let mut query = QueryBuilder::<Sqlite>::new(
        "SELECT seq, id, at, project_id, action, outcome, actor_kind, actor_id, via, \
         credential_id, target_kind, target_id, details, ip, user_agent FROM audit_events WHERE ",
    );
    match scope {
        Scope::Org(org) => {
            query.push("org_id = ").push_bind(org.to_string());
        }
        Scope::Instance => {
            query.push("org_id IS NULL");
        }
    }
    if let Some(from) = filters.from {
        query.push(" AND at >= ").push_bind(from);
    }
    if let Some(to) = filters.to {
        query.push(" AND at < ").push_bind(to);
    }
    if !filters.actions.is_empty() {
        query.push(" AND (");
        for (index, matcher) in filters.actions.iter().enumerate() {
            if index > 0 {
                query.push(" OR ");
            }
            match matcher {
                ActionMatch::Exact(name) => {
                    query.push("action = ").push_bind(name.clone());
                }
                ActionMatch::Prefix(prefix) => {
                    query
                        .push("action LIKE ")
                        .push_bind(format!("{}%", escape_like(prefix)))
                        .push(" ESCAPE '\\'");
                }
            }
        }
        query.push(")");
    }
    if let Some(actor) = &filters.actor_id {
        query.push(" AND actor_id = ").push_bind(actor.clone());
    }
    if let Some(target) = &filters.target_id {
        query.push(" AND target_id = ").push_bind(target.clone());
    }
    if let Some(project) = filters.project_id {
        query.push(" AND project_id = ").push_bind(project.to_string());
    }
    if let Some(outcome) = filters.outcome {
        query.push(" AND outcome = ").push_bind(outcome);
    }
    if let Some(before) = before_seq {
        query.push(" AND seq < ").push_bind(before);
    }
    query
        .push(" ORDER BY seq DESC LIMIT ")
        .push_bind(i64::from(limit));
    query
        .build_query_as::<Row>()
        .fetch_all(pool)
        .await
        .map_err(MetaError::from)
}

/// The display names of one page, read once per kind (plan P-3).
#[derive(Default)]
struct Names {
    /// `users` rows by id.
    users: HashMap<String, UserName>,
    /// Ids of the org's current members (org scope only).
    members: HashSet<String>,
    /// `api_keys` rows of the org's projects, by id; the name may be null (org scope only).
    keys: HashMap<String, Option<String>>,
    /// `projects` rows of the org, by id (org scope only).
    projects: HashMap<String, String>,
    /// `org_sso_providers` rows of the org, by id (org scope only).
    providers: HashMap<String, String>,
}

struct UserName {
    name: Option<String>,
    email: String,
}

/// A name, an email and a status, as an actor or a reference shows them.
type Resolved = (Option<String>, Option<String>, AuditRefStatus);

/// Turns one page of rows into API records, with names resolved (plan P-3).
///
/// # Errors
/// A stored id or outcome that does not decode, or a failed lookup.
pub(crate) async fn resolve(
    pool: &SqlitePool,
    scope: &Scope,
    rows: &[Row],
) -> Result<Vec<AuditEvent>, MetaError> {
    let names = lookups(pool, scope, rows).await?;
    rows.iter()
        .map(|row| event(row, scope, &names))
        .collect()
}

/// `IN (…)` over `ids`, with each id bound as text.
fn push_in(builder: &mut QueryBuilder<'_, Sqlite>, ids: &BTreeSet<String>) {
    builder.push("(");
    let mut separated = builder.separated(", ");
    for id in ids {
        separated.push_bind(id.clone());
    }
    separated.push_unseparated(")");
}

async fn lookups(pool: &SqlitePool, scope: &Scope, rows: &[Row]) -> Result<Names, MetaError> {
    let mut user_ids = BTreeSet::new();
    let mut key_ids = BTreeSet::new();
    let mut project_ids = BTreeSet::new();
    let mut provider_ids = BTreeSet::new();
    for row in rows {
        if let Some(id) = &row.actor_id {
            match row.actor_kind.as_str() {
                "user" => {
                    user_ids.insert(id.clone());
                }
                "api_key" => {
                    key_ids.insert(id.clone());
                }
                _ => {}
            }
        }
        if let (Some(kind), Some(id)) = (&row.target_kind, &row.target_id) {
            match kind.as_str() {
                "user" => {
                    user_ids.insert(id.clone());
                }
                "api_key" => {
                    key_ids.insert(id.clone());
                }
                "project" => {
                    project_ids.insert(id.clone());
                }
                "sso_provider" => {
                    provider_ids.insert(id.clone());
                }
                _ => {}
            }
        }
        if let Some(project) = &row.project_id {
            project_ids.insert(project.clone());
        }
    }

    let mut names = Names::default();
    if !user_ids.is_empty() {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT id, name, email FROM users WHERE id IN ");
        push_in(&mut builder, &user_ids);
        let found: Vec<(String, Option<String>, String)> = builder
            .build_query_as()
            .fetch_all(pool)
            .await
            .map_err(MetaError::from)?;
        for (id, name, email) in found {
            names.users.insert(id, UserName { name, email });
        }
    }

    // Org objects are resolved in an org log only: the instance log names none of them
    // (plan P-3, D-EA-8).
    let Scope::Org(org) = scope else {
        return Ok(names);
    };
    let org = org.to_string();
    if !user_ids.is_empty() {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT user_id FROM org_members WHERE org_id = ");
        builder.push_bind(org.clone()).push(" AND user_id IN ");
        push_in(&mut builder, &user_ids);
        let found: Vec<(String,)> = builder
            .build_query_as()
            .fetch_all(pool)
            .await
            .map_err(MetaError::from)?;
        names.members = found.into_iter().map(|(id,)| id).collect();
    }
    if !key_ids.is_empty() {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT k.id, k.name FROM api_keys k JOIN projects p ON p.id = k.project_id \
             WHERE p.org_id = ",
        );
        builder.push_bind(org.clone()).push(" AND k.id IN ");
        push_in(&mut builder, &key_ids);
        let found: Vec<(String, Option<String>)> = builder
            .build_query_as()
            .fetch_all(pool)
            .await
            .map_err(MetaError::from)?;
        names.keys = found.into_iter().collect();
    }
    if !project_ids.is_empty() {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT id, name FROM projects WHERE org_id = ");
        builder.push_bind(org.clone()).push(" AND id IN ");
        push_in(&mut builder, &project_ids);
        let found: Vec<(String, String)> = builder
            .build_query_as()
            .fetch_all(pool)
            .await
            .map_err(MetaError::from)?;
        names.projects = found.into_iter().collect();
    }
    if !provider_ids.is_empty() {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT id, display_name FROM org_sso_providers WHERE org_id = ",
        );
        builder.push_bind(org).push(" AND id IN ");
        push_in(&mut builder, &provider_ids);
        let found: Vec<(String, String)> = builder
            .build_query_as()
            .fetch_all(pool)
            .await
            .map_err(MetaError::from)?;
        names.providers = found.into_iter().collect();
    }
    Ok(names)
}

fn corrupt(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "audit_events",
        column,
    }
}

fn event(row: &Row, scope: &Scope, names: &Names) -> Result<AuditEvent, MetaError> {
    let id = Uuid::parse_str(&row.id).map_err(|_| corrupt("id"))?;
    let outcome = match row.outcome.as_str() {
        "success" => AuditOutcome::Success,
        "failure" => AuditOutcome::Failure,
        _ => return Err(corrupt("outcome")),
    };
    let details: serde_json::Value = match serde_json::from_str(&row.details) {
        Ok(value) => value,
        Err(_) => {
            tracing::warn!(id = %id, "audit details do not decode");
            serde_json::json!({})
        }
    };
    let target = match (row.target_kind.as_deref(), row.target_id.as_deref()) {
        (Some(kind), Some(target)) => Some(object_ref(kind, target, scope, names)),
        _ => None,
    };
    Ok(AuditEvent {
        id,
        at: row.at,
        action: row.action.clone(),
        outcome,
        actor: actor(row, scope, names),
        target,
        project: row
            .project_id
            .as_deref()
            .map(|project| object_ref("project", project, scope, names)),
        details,
        ip: row.ip.clone(),
        user_agent: row.user_agent.clone(),
    })
}

fn actor(row: &Row, scope: &Scope, names: &Names) -> AuditActor {
    let (name, email, status) = match (row.actor_kind.as_str(), row.actor_id.as_deref()) {
        ("user", Some(id)) => user_ref(id, scope, names),
        ("api_key", Some(id)) => key_ref(id, scope, names),
        _ => (None, None, AuditRefStatus::Unresolved),
    };
    AuditActor {
        kind: row.actor_kind.clone(),
        id: row.actor_id.clone(),
        via: row.via.clone(),
        credential_id: row.credential_id.clone(),
        name,
        email,
        status,
    }
}

fn object_ref(kind: &str, id: &str, scope: &Scope, names: &Names) -> AuditRef {
    let (name, email, status) = match kind {
        "user" => user_ref(id, scope, names),
        "api_key" => key_ref(id, scope, names),
        "project" => project_ref(id, scope, names),
        "sso_provider" => provider_ref(id, scope, names),
        _ => (None, None, AuditRefStatus::Unresolved),
    };
    AuditRef {
        kind: kind.to_owned(),
        id: id.to_owned(),
        name,
        email,
        status,
    }
}

/// A user: no row is `deleted` (erased); in an org log, a user who is not a member is
/// `former_member`. Only a present user shows a name and an email.
fn user_ref(id: &str, scope: &Scope, names: &Names) -> Resolved {
    let Some(user) = names.users.get(id) else {
        return (None, None, AuditRefStatus::Deleted);
    };
    if matches!(scope, Scope::Org(_)) && !names.members.contains(id) {
        return (None, None, AuditRefStatus::FormerMember);
    }
    (
        user.name.clone(),
        Some(user.email.clone()),
        AuditRefStatus::Present,
    )
}

/// An API key of the org's projects: its name, no email. Unresolved in the instance log.
fn key_ref(id: &str, scope: &Scope, names: &Names) -> Resolved {
    match scope {
        Scope::Instance => (None, None, AuditRefStatus::Unresolved),
        Scope::Org(_) => match names.keys.get(id) {
            Some(name) => (name.clone(), None, AuditRefStatus::Present),
            None => (None, None, AuditRefStatus::Deleted),
        },
    }
}

fn project_ref(id: &str, scope: &Scope, names: &Names) -> Resolved {
    match scope {
        Scope::Instance => (None, None, AuditRefStatus::Unresolved),
        Scope::Org(_) => match names.projects.get(id) {
            Some(name) => (Some(name.clone()), None, AuditRefStatus::Present),
            None => (None, None, AuditRefStatus::Deleted),
        },
    }
}

fn provider_ref(id: &str, scope: &Scope, names: &Names) -> Resolved {
    match scope {
        Scope::Instance => (None, None, AuditRefStatus::Unresolved),
        Scope::Org(_) => match names.providers.get(id) {
            Some(name) => (Some(name.clone()), None, AuditRefStatus::Present),
            None => (None, None, AuditRefStatus::Deleted),
        },
    }
}

/// The `seq` cursor of a page: the base64url (no padding) of `{"seq":N}` (plan P-1).
#[derive(Debug, Serialize, Deserialize)]
struct StoredCursor {
    seq: i64,
}

/// The cursor that continues a page after the record `seq`.
pub(crate) fn encode_cursor(seq: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("{{\"seq\":{seq}}}"))
}

/// The `seq` a cursor continues after.
///
/// # Errors
/// A cursor that is not one of [`encode_cursor`]'s: `field_error("cursor", "invalid cursor")`.
pub(crate) fn decode_cursor(text: &str) -> Result<i64, ApiError> {
    let invalid = || field_error("cursor", "invalid cursor");
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
    let stored: StoredCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    Ok(stored.seq)
}
