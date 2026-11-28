//! Notification channels (spec 07 § Channels, D-K-5, D-K-6).

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, patch, post};
use cetra_core::ids::{OrgId, ProjectId};
use serde::Deserialize;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::alerts::channel::{ChannelConfig, open_config, seal_config};
use crate::alerts::deliver::{Outgoing, send_to_channel};
use crate::alerts::message::{self, MessageKind, Notice};
use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event as CatalogEvent;
use crate::analytics::record;
use crate::api::alerts::{
    AlertMetric, AlertWindow, ChannelDelivery, ChannelKind, ChannelTestResult, Comparator,
    CreateChannelRequest, DeliveryStatus, NotificationChannel, UpdateChannelRequest,
};
use crate::api::common::{FieldError, Page};
use crate::audit::catalog::AlertChannelAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::config::EmailRecipients;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;
use crate::meta::alerts::{self, ChannelPatch, ChannelRow, LastDelivery, NewChannel};
use crate::meta::members::verified_member_emails;
use crate::meta::{MetaError, orgs};

/// Channel routes. Reads need `view`; writes need `manage_alerts` (D-K-9).
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/channels",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_channels),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/channels",
            access: Access::Project {
                perm: ProjectPerm::ManageAlerts,
                session_only: false,
            },
            documented: true,
            handler: post(create_channel),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/channels/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageAlerts,
                session_only: false,
            },
            documented: true,
            handler: patch(update_channel),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/channels/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageAlerts,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_channel),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/channels/{id}/test",
            access: Access::Project {
                perm: ProjectPerm::ManageAlerts,
                session_only: false,
            },
            documented: true,
            handler: post(test_channel),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct ChannelPath {
    id: Uuid,
}

/// Not paged: a project has at most 20 channels (D-K-5).
async fn list_channels(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
) -> Result<error::Json<Page<NotificationChannel>>, ApiError> {
    let project_id = access.project.id;
    let rows = alerts::list_channels(state.meta.reader(), project_id).await?;
    let last = alerts::last_deliveries(state.meta.reader(), project_id).await?;
    let data = rows
        .iter()
        .map(|row| channel_json(&state, row, last.get(&row.id)))
        .collect();
    Ok(error::Json(Page {
        data,
        next_cursor: None,
    }))
}

async fn create_channel(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateChannelRequest>,
) -> Result<(StatusCode, error::Json<NotificationChannel>), ApiError> {
    principal::session_user(&principal)?;
    let name = checked_name(&body.name)?;
    if body.kind == ChannelKind::Email && state.mailer.is_none() {
        return Err(field_error(
            "kind",
            "email channels need SMTP (smtp_url is not set)",
        ));
    }
    let config = config_from_create(&body)?;
    config.validate().map_err(|errors| ApiError::BadRequest {
        detail: None,
        errors,
    })?;
    let members = member_scope(&state, access.project.org_id, body.kind).await?;
    check_members(&config, members.as_deref())?;
    let id = Uuid::now_v7();
    let config_enc = seal_config(&state.secrets, &id, &config)
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let target_hint = config.target_hint();
    let project_id = access.project.id;
    let kind = body.kind;
    let org_id = access.project.org_id;
    let now = state.clock.now_ns();
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let row = alerts::insert_channel(
                    conn,
                    NewChannel {
                        id,
                        project_id,
                        kind,
                        name,
                        config_enc,
                        target_hint,
                    },
                    now,
                )
                .await?;
                let event = Event::new(AlertChannelAction::Created {
                    kind: row.kind.as_str(),
                })
                .org(org_id)
                .project(project_id)
                .target(TargetKind::AlertChannel, row.id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(row)
            })
        })
        .await?;
    record(
        &state,
        &CatalogEvent::ChannelCreated { kind },
        &Actor::project(&principal, &access),
    );
    Ok((
        StatusCode::CREATED,
        error::Json(channel_json(&state, &row, None)),
    ))
}

async fn update_channel(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(ChannelPath { id }): error::Path<ChannelPath>,
    error::Json(body): error::Json<UpdateChannelRequest>,
) -> Result<error::Json<NotificationChannel>, ApiError> {
    principal::session_user(&principal)?;
    let project_id = access.project.id;
    let org_id = access.project.org_id;
    // The JSON names of the body's `Some` fields (plan P-2): names only, never values.
    let fields = [
        body.name.is_some().then_some("name"),
        body.url.is_some().then_some("url"),
        body.secret.is_some().then_some("secret"),
        body.headers.is_some().then_some("headers"),
        body.recipients.is_some().then_some("recipients"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<&'static str>>();
    let now = state.clock.now_ns();
    let members = if body.recipients.is_some() {
        member_scope(&state, access.project.org_id, ChannelKind::Email).await?
    } else {
        None
    };
    // The read, the open, the apply, the validation, the seal and the write share one
    // transaction, so two PATCHes never start from the same stored config.
    let handle = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let patched = patch_channel_on(
                    conn,
                    &handle,
                    project_id,
                    id,
                    &body,
                    now,
                    members.as_deref(),
                )
                .await?;
                if let Ok(Some(row)) = &patched {
                    let event = Event::new(AlertChannelAction::Updated { fields })
                        .org(org_id)
                        .project(project_id)
                        .target(TargetKind::AlertChannel, row.id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(patched)
            })
        })
        .await?;
    let row = outcome?.ok_or(ApiError::NotFound)?;
    let last = alerts::last_deliveries(state.meta.reader(), project_id).await?;
    Ok(error::Json(channel_json(&state, &row, last.get(&row.id))))
}

async fn delete_channel(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(ChannelPath { id }): error::Path<ChannelPath>,
) -> Result<StatusCode, ApiError> {
    principal::session_user(&principal)?;
    let project_id = access.project.id;
    let org_id = access.project.org_id;
    let now = state.clock.now_ns();
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let deleted = alerts::delete_channel(conn, project_id, id).await?;
                if deleted {
                    let event = Event::new(AlertChannelAction::Deleted)
                        .org(org_id)
                        .project(project_id)
                        .target(TargetKind::AlertChannel, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(deleted)
            })
        })
        .await?;
    if !deleted {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Sends one sample message now and reports the result (D-K-7). Nothing is stored.
async fn test_channel(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ChannelPath { id }): error::Path<ChannelPath>,
) -> Result<error::Json<ChannelTestResult>, ApiError> {
    refuse_outbound(&state, "channel test")?;
    let user_id = principal::session_user(&principal)?.id;
    let project = &access.project;
    let now = state.clock.now_ns();
    // Five sends a minute per user and five per channel (D-K-19). The channel is looked up in
    // this project first, so a channel of another project (a 404) spends no budget of its own.
    let limited = |retry: crate::auth::limits::RetryAfter| ApiError::RateLimited {
        retry_after_s: retry.0.as_secs().max(1),
    };
    state
        .limits
        .alert_tests_by_user
        .check(&user_id, now)
        .map_err(limited)?;
    let Some(channel) = alerts::channel_by_id(state.meta.reader(), project.id, id).await? else {
        return Err(ApiError::NotFound);
    };
    state
        .limits
        .alert_tests_by_channel
        .check(&channel.id, now)
        .map_err(limited)?;
    let config = match open_config(&state.secrets, &channel.id, &channel.config_enc) {
        Ok(config) => config,
        Err(_) => {
            return Ok(error::Json(ChannelTestResult {
                ok: false,
                error: Some("the channel config could not be read".to_owned()),
            }));
        }
    };
    let org = orgs::org_by_id(state.meta.reader(), project.org_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let window_from = now.saturating_sub(300_000_000_000);
    let public_url = &state.config.public_url;
    let notice = Notice {
        kind: MessageKind::Test,
        event_id: Uuid::now_v7(),
        rule_id: Uuid::nil(),
        rule_name: "Test alert".to_owned(),
        metric: AlertMetric::ErrorRate,
        comparator: Comparator::Gt,
        threshold: 0.05,
        window: AlertWindow::M5,
        value: Some(0.125),
        count: Some(48),
        window_from,
        window_to: now,
        occurred_at: now,
        project_id: project.id,
        project_name: project.name.clone(),
        rule_url: public_url.join(&format!("/{}/{}/alerts", org.slug, project.slug)),
        traces_url: message::traces_url(
            public_url,
            &org.slug,
            &project.slug,
            window_from,
            now,
            None,
        ),
    };
    let sent = send_to_channel(
        &state,
        project.org_id,
        &config,
        Outgoing::Alert(&notice),
        Uuid::now_v7(),
        now,
    );
    let result = match Box::pin(sent).await {
        Ok(()) => ChannelTestResult {
            ok: true,
            error: None,
        },
        Err(error) => ChannelTestResult {
            ok: false,
            error: Some(error.message),
        },
    };
    tracing::info!(
        channel_id = %id,
        project_id = %project.id,
        user_id = %user_id,
        ok = result.ok,
        "alert channel tested"
    );
    Ok(error::Json(result))
}

/// Whether `field` is a config field of `kind` (the request field names).
fn applies(kind: ChannelKind, field: &str) -> bool {
    matches!(
        (kind, field),
        (ChannelKind::Slack, "url")
            | (ChannelKind::Webhook, "url" | "secret" | "headers")
            | (ChannelKind::Email, "recipients")
    )
}

/// Whether a create of `kind` needs `field`. A webhook's headers are optional.
fn required(kind: ChannelKind, field: &str) -> bool {
    matches!(
        (kind, field),
        (ChannelKind::Slack, "url")
            | (ChannelKind::Webhook, "url" | "secret")
            | (ChannelKind::Email, "recipients")
    )
}

/// The config of a create body. Fields that do not belong to the kind are refused, and so is a
/// missing required field. Every problem is reported, in field order.
fn config_from_create(body: &CreateChannelRequest) -> Result<ChannelConfig, ApiError> {
    let kind = body.kind;
    let fields = [
        ("url", body.url.is_some()),
        ("secret", body.secret.is_some()),
        ("headers", body.headers.is_some()),
        ("recipients", body.recipients.is_some()),
    ];
    let mut errors = Vec::new();
    for (field, present) in fields {
        if !applies(kind, field) && present {
            errors.push(field_item(
                field,
                &format!("does not apply to {}", kind.as_str()),
            ));
        } else if required(kind, field) && !present {
            errors.push(field_item(field, "is required"));
        }
    }
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    Ok(match kind {
        ChannelKind::Slack => ChannelConfig::Slack {
            url: body.url.clone().unwrap_or_default(),
        },
        ChannelKind::Webhook => ChannelConfig::Webhook {
            url: body.url.clone().unwrap_or_default(),
            secret: body.secret.clone().unwrap_or_default(),
            headers: body.headers.clone().unwrap_or_default(),
        },
        ChannelKind::Email => ChannelConfig::Email {
            recipients: body
                .recipients
                .iter()
                .flatten()
                .map(|entry| entry.trim().to_owned())
                .collect(),
        },
    })
}

/// The stored config with the fields of an update applied. A field of another kind is refused;
/// every such field is reported together.
fn apply_update(
    stored: ChannelConfig,
    body: &UpdateChannelRequest,
) -> Result<ChannelConfig, ApiError> {
    let kind = stored.kind();
    let fields = [
        ("url", body.url.is_some()),
        ("secret", body.secret.is_some()),
        ("headers", body.headers.is_some()),
        ("recipients", body.recipients.is_some()),
    ];
    let errors: Vec<FieldError> = fields
        .into_iter()
        .filter(|(field, present)| *present && !applies(kind, field))
        .map(|(field, _)| field_item(field, &format!("does not apply to {}", kind.as_str())))
        .collect();
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    Ok(match stored {
        ChannelConfig::Slack { url } => ChannelConfig::Slack {
            url: body.url.clone().unwrap_or(url),
        },
        ChannelConfig::Webhook {
            url,
            secret,
            headers,
        } => ChannelConfig::Webhook {
            url: body.url.clone().unwrap_or(url),
            secret: body.secret.clone().unwrap_or(secret),
            headers: body.headers.clone().unwrap_or(headers),
        },
        ChannelConfig::Email { recipients } => ChannelConfig::Email {
            recipients: body.recipients.as_ref().map_or(recipients, |list| {
                list.iter().map(|entry| entry.trim().to_owned()).collect()
            }),
        },
    })
}

/// Reads the channel, plans the PATCH and writes it, all on the caller's transaction. The outer
/// `Err` is a database failure, which rolls the transaction back. The inner `Err` is a refusal
/// found before any write; `Ok(None)` means the channel is not in the project.
async fn patch_channel_on(
    conn: &mut SqliteConnection,
    handle: &AppState,
    project_id: ProjectId,
    id: Uuid,
    body: &UpdateChannelRequest,
    now: i64,
    members: Option<&[String]>,
) -> Result<Result<Option<ChannelRow>, ApiError>, MetaError> {
    let Some(current) = alerts::channel_by_id_on(conn, project_id, id).await? else {
        return Ok(Ok(None));
    };
    let patch = match planned_patch(handle, &current, body, members) {
        Ok(patch) => patch,
        Err(error) => return Ok(Err(error)),
    };
    let row = alerts::update_channel(conn, project_id, id, patch, now).await?;
    Ok(Ok(row))
}

/// The patch of a PATCH: the trimmed name, and the sealed config when a config field is sent.
fn planned_patch(
    handle: &AppState,
    current: &ChannelRow,
    body: &UpdateChannelRequest,
    members: Option<&[String]>,
) -> Result<ChannelPatch, ApiError> {
    let name = body.name.as_deref().map(checked_name).transpose()?;
    let config = if body.url.is_some()
        || body.secret.is_some()
        || body.headers.is_some()
        || body.recipients.is_some()
    {
        let stored = open_config(&handle.secrets, &current.id, &current.config_enc)
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        let next = apply_update(stored, body)?;
        next.validate().map_err(|errors| ApiError::BadRequest {
            detail: None,
            errors,
        })?;
        check_members(&next, members)?;
        let sealed = seal_config(&handle.secrets, &current.id, &next)
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        Some((sealed, next.target_hint()))
    } else {
        None
    };
    Ok(ChannelPatch { name, config })
}

/// The verified member emails that the recipients of a `kind` channel must stay within, when
/// `alerts.email_recipients` is `members` (D-K-19, D-K-21). `None` when any address is allowed.
async fn member_scope(
    state: &AppState,
    org_id: OrgId,
    kind: ChannelKind,
) -> Result<Option<Vec<String>>, ApiError> {
    if kind != ChannelKind::Email || state.config.email_recipients() != EmailRecipients::Members {
        return Ok(None);
    }
    Ok(Some(
        verified_member_emails(state.meta.reader(), org_id).await?,
    ))
}

/// When the policy is `members`, each recipient of an email config must be a verified member of
/// the org (D-K-19, D-K-21). The error names the field, not the address.
fn check_members(config: &ChannelConfig, members: Option<&[String]>) -> Result<(), ApiError> {
    let (ChannelConfig::Email { recipients }, Some(members)) = (config, members) else {
        return Ok(());
    };
    let all_members = recipients.iter().all(|recipient| {
        members
            .iter()
            .any(|member| member.eq_ignore_ascii_case(recipient))
    });
    if all_members {
        Ok(())
    } else {
        Err(field_error(
            "recipients",
            "must be verified members of this organization",
        ))
    }
}

/// A trimmed channel name of 1 to 100 characters.
fn checked_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(field_error("name", "is required"));
    }
    if name.chars().count() > 100 {
        return Err(field_error("name", "at most 100 characters"));
    }
    Ok(name.to_owned())
}

/// The response body of a channel. The config is opened only for the email recipients and the
/// header names; a config that does not open still lists (the list must answer).
fn channel_json(
    state: &AppState,
    row: &ChannelRow,
    last: Option<&LastDelivery>,
) -> NotificationChannel {
    let (recipients, header_names) = match open_config(&state.secrets, &row.id, &row.config_enc) {
        Ok(ChannelConfig::Email { recipients }) => (recipients, Vec::new()),
        Ok(ChannelConfig::Webhook { headers, .. }) => (Vec::new(), headers.into_keys().collect()),
        Ok(ChannelConfig::Slack { .. }) => (Vec::new(), Vec::new()),
        Err(_) => {
            tracing::warn!(channel_id = %row.id, "channel config does not open");
            (Vec::new(), Vec::new())
        }
    };
    let last_delivery = last.and_then(|last| {
        Some(ChannelDelivery {
            status: DeliveryStatus::parse(&last.status)?,
            at: last.at,
            error: last.error.clone(),
        })
    });
    NotificationChannel {
        id: row.id,
        name: row.name.clone(),
        kind: row.kind,
        target_hint: row.target_hint.clone(),
        available: row.kind != ChannelKind::Email || state.mailer.is_some(),
        recipients,
        header_names,
        last_delivery,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn field_item(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

fn field_error(field: &str, message: &str) -> ApiError {
    bad_request(vec![field_item(field, message)])
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use serde_json::{Value, json};

    use super::*;
    use crate::alerts::quota::DAILY_EMAIL_LIMIT;
    use crate::api::account::SignupPolicy;
    use crate::api::alerts::{AlertCondition, AlertMetric, AlertWindow, Comparator};
    use crate::auth::roles::OrgRole;
    use crate::config::EmailRecipients;
    use crate::meta::members::remove_org_member;
    use crate::test_support::hook_receiver::HookReceiver;
    use crate::test_support::{Seeded, TestApp, TestResponse, assert_logs_clean};

    const SECRET: &str = "0123456789abcdef";
    const SLACK_URL: &str = "https://hooks.slack.com/services/T0/B0/XXXXabcd";
    const WEBHOOK_URL: &str = "https://hooks.example.com/x";

    fn channels(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}/channels", seeded.project.id)
    }

    fn pager() -> Value {
        json!({
            "name": "Pager",
            "kind": "webhook",
            "url": WEBHOOK_URL,
            "secret": SECRET,
            "headers": {"X-Team": "core"},
        })
    }

    /// Sends one request with a session cookie.
    async fn send(
        app: &TestApp,
        method: Method,
        path: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> TestResponse {
        let mut request = app
            .request(method, path)
            .header(header::COOKIE.as_str(), cookie);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await
    }

    /// Creates a channel as the owner and expects `201`.
    async fn create_ok(app: &TestApp, seeded: &Seeded, body: Value) -> NotificationChannel {
        let response = send(
            app,
            Method::POST,
            &channels(seeded),
            &seeded.cookie,
            Some(body),
        )
        .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    /// The `(field, message)` pairs of a problem response.
    fn problem_of(response: &TestResponse) -> Vec<(String, String)> {
        response
            .problem()
            .errors
            .into_iter()
            .map(|error| (error.field, error.message))
            .collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(field, message)| ((*field).to_owned(), (*message).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn create_and_list_each_kind() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        create_ok(
            &app,
            &seeded,
            json!({"name": "Team Slack", "kind": "slack", "url": SLACK_URL}),
        )
        .await;
        create_ok(&app, &seeded, pager()).await;
        create_ok(
            &app,
            &seeded,
            json!({"name": "Mail", "kind": "email", "recipients": ["oncall@example.com", " ada@example.com "]}),
        )
        .await;

        let list = send(&app, Method::GET, &channels(&seeded), &seeded.cookie, None).await;
        assert_eq!(list.status, 200);
        let text = String::from_utf8_lossy(&list.body).into_owned();
        for needle in ["XXXXabcd", "services/T0", SECRET, "\"core\""] {
            assert!(!text.contains(needle), "list contains {needle}");
        }
        let page: Page<NotificationChannel> = list.json();
        let names: Vec<&str> = page.data.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Mail", "Pager", "Team Slack"]);
        assert_eq!(
            page.data[0].recipients,
            ["oncall@example.com", "ada@example.com"]
        );
        assert_eq!(page.data[1].header_names, ["X-Team"]);
        assert_eq!(page.data[2].target_hint, "hooks.slack.com/…/abcd");
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn config_is_sealed_at_rest() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, pager()).await;
        let sealed =
            sqlx::query_scalar::<_, Vec<u8>>("SELECT config_enc FROM notification_channels")
                .fetch_all(app.state.meta.reader())
                .await
                .unwrap();
        assert_eq!(sealed.len(), 1);
        assert!(
            !sealed[0]
                .windows(SECRET.len())
                .any(|window| window == SECRET.as_bytes())
        );
    }

    #[tokio::test]
    async fn fields_of_another_kind_are_refused() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        let path = channels(&seeded);
        let slack = send(
            &app,
            Method::POST,
            &path,
            &seeded.cookie,
            Some(json!({"name": "S", "kind": "slack", "url": SLACK_URL, "secret": "x"})),
        )
        .await;
        assert_eq!(slack.status, 400);
        assert_eq!(
            problem_of(&slack),
            pairs(&[("secret", "does not apply to slack")])
        );

        let email = send(
            &app,
            Method::POST,
            &path,
            &seeded.cookie,
            Some(json!({"name": "M", "kind": "email"})),
        )
        .await;
        assert_eq!(email.status, 400);
        assert_eq!(problem_of(&email), pairs(&[("recipients", "is required")]));

        let short = send(
            &app,
            Method::POST,
            &path,
            &seeded.cookie,
            Some(json!({"name": "W", "kind": "webhook", "url": WEBHOOK_URL, "secret": "0123456789abcde"})),
        )
        .await;
        assert_eq!(short.status, 400);
        assert_eq!(
            problem_of(&short),
            pairs(&[("secret", "must be 16 to 256 characters")])
        );
    }

    #[tokio::test]
    async fn email_needs_smtp() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = send(
            &app,
            Method::POST,
            &channels(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Mail", "kind": "email", "recipients": ["a@x.io"]})),
        )
        .await;
        assert_eq!(response.status, 400);
        assert_eq!(
            problem_of(&response),
            pairs(&[("kind", "email channels need SMTP (smtp_url is not set)")])
        );

        // A channel stored before SMTP was removed still lists, unavailable.
        let id = Uuid::now_v7();
        let project_id = seeded.project.id;
        let now = app.state.clock.now_ns();
        let sealed = seal_config(
            &app.state.secrets,
            &id,
            &ChannelConfig::Email {
                recipients: vec!["a@x.io".to_owned()],
            },
        )
        .unwrap();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    alerts::insert_channel(
                        conn,
                        NewChannel {
                            id,
                            project_id,
                            kind: ChannelKind::Email,
                            name: "Stored".to_owned(),
                            config_enc: sealed,
                            target_hint: "a@x.io".to_owned(),
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let list = send(&app, Method::GET, &channels(&seeded), &seeded.cookie, None).await;
        let page: Page<NotificationChannel> = list.json();
        assert_eq!(page.data.len(), 1);
        assert!(!page.data[0].available);
    }

    #[tokio::test]
    async fn names_are_unique() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, pager()).await;
        let again = send(
            &app,
            Method::POST,
            &channels(&seeded),
            &seeded.cookie,
            Some(pager()),
        )
        .await;
        assert_eq!(again.status, 409);
    }

    #[tokio::test]
    async fn update_keeps_absent_fields() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, pager()).await;
        let path = format!("{}/{}", channels(&seeded), created.id);

        let renamed = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"name": "Pager 2"})),
        )
        .await;
        assert_eq!(renamed.status, 200);
        let renamed: NotificationChannel = renamed.json();
        assert_eq!(renamed.name, "Pager 2");
        assert_eq!(renamed.header_names, ["X-Team"]);

        let cleared = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"headers": {}})),
        )
        .await;
        let cleared: NotificationChannel = cleared.json();
        assert!(cleared.header_names.is_empty());

        let refused = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"recipients": ["a@x.io"]})),
        )
        .await;
        assert_eq!(refused.status, 400);
        assert_eq!(
            problem_of(&refused),
            pairs(&[("recipients", "does not apply to webhook")])
        );

        let short = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"secret": ""})),
        )
        .await;
        assert_eq!(short.status, 400);
        assert_eq!(
            problem_of(&short),
            pairs(&[("secret", "must be 16 to 256 characters")])
        );

        let row = alerts::channel_by_id(app.state.meta.reader(), seeded.project.id, created.id)
            .await
            .unwrap()
            .unwrap();
        let ChannelConfig::Webhook { secret, .. } =
            open_config(&app.state.secrets, &row.id, &row.config_enc).unwrap()
        else {
            panic!("the channel is a webhook");
        };
        assert_eq!(secret, SECRET);
    }

    #[tokio::test]
    async fn concurrent_patches_keep_each_change() {
        // Two PATCHes of different fields sent at once must both persist: each reads,
        // edits and re-seals the config inside one write transaction. The requests can
        // interleave at every await, so several rounds make a lost update show up.
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let new_url = "https://hooks.example.com/y";
        let new_secret = "fedcba9876543210";
        for round in 0..20 {
            let mut body = pager();
            body["name"] = json!(format!("Pager {round}"));
            let created = create_ok(&app, &seeded, body).await;
            let path = format!("{}/{}", channels(&seeded), created.id);
            let (by_url, by_secret) = tokio::join!(
                send(
                    &app,
                    Method::PATCH,
                    &path,
                    &seeded.cookie,
                    Some(json!({"url": new_url})),
                ),
                send(
                    &app,
                    Method::PATCH,
                    &path,
                    &seeded.cookie,
                    Some(json!({"secret": new_secret})),
                ),
            );
            assert_eq!(by_url.status, 200);
            assert_eq!(by_secret.status, 200);

            let row = alerts::channel_by_id(app.state.meta.reader(), seeded.project.id, created.id)
                .await
                .unwrap()
                .unwrap();
            let ChannelConfig::Webhook { url, secret, .. } =
                open_config(&app.state.secrets, &row.id, &row.config_enc).unwrap()
            else {
                panic!("the channel is a webhook");
            };
            assert_eq!(url, new_url, "round {round}");
            assert_eq!(secret, new_secret, "round {round}");
        }
    }

    #[tokio::test]
    async fn update_and_delete_unknown_ids_are_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let unknown = "01900000-0000-7000-8000-000000009999";
        let path = format!("{}/{unknown}", channels(&seeded));
        let patched = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"name": "x"})),
        )
        .await;
        assert_eq!(patched.status, 404);
        let deleted = send(&app, Method::DELETE, &path, &seeded.cookie, None).await;
        assert_eq!(deleted.status, 404);
    }

    #[tokio::test]
    async fn delete_unlinks_rules() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let channel = create_ok(&app, &seeded, pager()).await;
        let condition = AlertCondition {
            metric: AlertMetric::ErrorRate,
            window: AlertWindow::M5,
            filter: None,
            comparator: Comparator::Gt,
            threshold: 0.5,
            min_count: Some(20),
            target: None,
            score_name: None,
            aggregation: None,
            issue_id: None,
            source_id: None,
        };
        let rule = alerts::NewRule {
            project_id: seeded.project.id,
            name: "r".to_owned(),
            condition,
            channel_ids: vec![channel.id],
            enabled: true,
            cooldown_s: 3600,
            notify_while_firing: false,
            created_by: None,
        };
        let now = app.state.clock.now_ns();
        let rule_id = app
            .state
            .meta
            .write(move |conn| Box::pin(async move { alerts::insert_rule(conn, rule, now).await }))
            .await
            .unwrap()
            .id;
        let path = format!("{}/{}", channels(&seeded), channel.id);
        let deleted = send(&app, Method::DELETE, &path, &seeded.cookie, None).await;
        assert_eq!(deleted.status, 204);
        let loaded = alerts::rule_by_id(app.state.meta.reader(), seeded.project.id, rule_id)
            .await
            .unwrap()
            .unwrap();
        assert!(loaded.channel_ids.is_empty());
    }

    #[tokio::test]
    async fn roles_and_keys() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let channel = create_ok(&app, &seeded, pager()).await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let path = format!("{}/{}", channels(&seeded), channel.id);

        let listed = send(&app, Method::GET, &channels(&seeded), &viewer.cookie, None).await;
        assert_eq!(listed.status, 200);
        let created = send(
            &app,
            Method::POST,
            &channels(&seeded),
            &viewer.cookie,
            Some(json!({"name": "V", "kind": "slack", "url": SLACK_URL})),
        )
        .await;
        assert_eq!(created.status, 403);
        let patched = send(
            &app,
            Method::PATCH,
            &path,
            &viewer.cookie,
            Some(json!({"name": "V"})),
        )
        .await;
        assert_eq!(patched.status, 403);
        let deleted = send(&app, Method::DELETE, &path, &viewer.cookie, None).await;
        assert_eq!(deleted.status, 403);

        let by_member = send(
            &app,
            Method::POST,
            &channels(&seeded),
            &member.cookie,
            Some(json!({"name": "M", "kind": "slack", "url": SLACK_URL})),
        )
        .await;
        assert_eq!(by_member.status, 201);

        let bearer = format!("Bearer {}", seeded.read_key);
        let key_list = app
            .request(Method::GET, &channels(&seeded))
            .header(header::AUTHORIZATION.as_str(), &bearer)
            .send()
            .await;
        assert_eq!(key_list.status, 200);
        let key_create = app
            .request(Method::POST, &channels(&seeded))
            .header(header::AUTHORIZATION.as_str(), &bearer)
            .json(&json!({"name": "K", "kind": "slack", "url": SLACK_URL}))
            .send()
            .await;
        assert_eq!(key_create.status, 403);
    }

    #[tokio::test]
    async fn send_test_reports_the_result() {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        let accepting = HookReceiver::start(vec![200]).await;
        let refusing = HookReceiver::start(vec![500]).await;
        let good = create_ok(
            &app,
            &seeded,
            json!({"name": "Good", "kind": "webhook", "url": accepting.url("/hook"), "secret": SECRET}),
        )
        .await;
        let bad = create_ok(
            &app,
            &seeded,
            json!({"name": "Bad", "kind": "webhook", "url": refusing.url("/hook"), "secret": SECRET}),
        )
        .await;
        let test_path = |id: Uuid| format!("{}/{id}/test", channels(&seeded));

        let ok = send(
            &app,
            Method::POST,
            &test_path(good.id),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(ok.status, 200);
        assert_eq!(ok.json::<Value>(), json!({"ok": true}));
        let received = accepting.received();
        assert_eq!(received.len(), 1);
        assert!(String::from_utf8_lossy(&received[0].body).contains("\"type\":\"alert.test\""));

        let failed = send(&app, Method::POST, &test_path(bad.id), &seeded.cookie, None).await;
        assert_eq!(failed.status, 200);
        assert_eq!(
            failed.json::<Value>(),
            json!({"ok": false, "error": "the endpoint answered 500"})
        );

        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let denied = send(
            &app,
            Method::POST,
            &test_path(good.id),
            &viewer.cookie,
            None,
        )
        .await;
        assert_eq!(denied.status, 403);

        let unknown = send(
            &app,
            Method::POST,
            &test_path(Uuid::from_u128(9)),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(unknown.status, 404);
    }

    #[tokio::test]
    async fn logs_hold_no_secrets() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        assert_logs_clean(
            &[SECRET, "hooks.example.com/x", "oncall@example.com"],
            async {
                let webhook = create_ok(&app, &seeded, pager()).await;
                let path = format!("{}/{}", channels(&seeded), webhook.id);
                let renamed = send(
                    &app,
                    Method::PATCH,
                    &path,
                    &seeded.cookie,
                    Some(json!({"name": "Pager 2"})),
                )
                .await;
                assert_eq!(renamed.status, 200);
                create_ok(
                    &app,
                    &seeded,
                    json!({"name": "Mail", "kind": "email", "recipients": ["oncall@example.com"]}),
                )
                .await;
            },
        )
        .await;
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT details FROM audit_events WHERE action LIKE 'alert_channel.%'")
                .fetch_all(app.state.meta.reader())
                .await
                .unwrap();
        assert_eq!(rows.len(), 3);
        for (details,) in rows {
            for secret in [SECRET, "hooks.example.com/x", "oncall@example.com"] {
                assert!(!details.contains(secret), "{details}");
            }
        }
    }

    #[tokio::test]
    async fn channel_update_records_field_names_only() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, pager()).await;
        let path = format!("{}/{}", channels(&seeded), created.id);
        let patched = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"name": "Pager 2", "url": "https://hooks.example.com/y"})),
        )
        .await;
        assert_eq!(patched.status, 200);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT details FROM audit_events WHERE action = 'alert_channel.updated' AND target_id = ?",
        )
        .bind(created.id.to_string())
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        let details: Value = serde_json::from_str(&rows[0].0).unwrap();
        assert_eq!(details["fields"], json!(["name", "url"]));
        assert!(!rows[0].0.contains("hooks.example.com/y"), "{}", rows[0].0);
    }

    /// An email channel to `recipients`, as a create body.
    fn email_body(name: &str, recipients: &[&str]) -> Value {
        json!({"name": name, "kind": "email", "recipients": recipients})
    }

    /// Sends one test; the response with its status.
    async fn test_send(app: &TestApp, id: Uuid, seeded: &Seeded, cookie: &str) -> TestResponse {
        let path = format!("{}/{id}/test", channels(seeded));
        send(app, Method::POST, &path, cookie, None).await
    }

    #[tokio::test]
    async fn test_sends_are_rate_limited_per_user_and_per_channel() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        let admin = app
            .add_member(&seeded, "admin@example.com", OrgRole::Admin)
            .await;
        let first = create_ok(&app, &seeded, email_body("First", &["oncall@example.com"])).await;
        let second = create_ok(&app, &seeded, email_body("Second", &["oncall@example.com"])).await;

        // Five sends of the first channel, split between two users.
        for _ in 0..3 {
            let sent = test_send(&app, first.id, &seeded, &seeded.cookie).await;
            assert_eq!(sent.status, 200);
        }
        for _ in 0..2 {
            let sent = test_send(&app, first.id, &seeded, &admin.cookie).await;
            assert_eq!(sent.status, 200);
        }
        // The sixth send of that channel is refused for whoever sends it (5 per channel).
        let refused = test_send(&app, first.id, &seeded, &admin.cookie).await;
        assert_eq!(refused.status, 429);
        assert!(refused.headers.get("retry-after").is_some());

        // The owner has spent three of five user sends; two more go, the next is refused (5 per
        // user, across channels).
        for _ in 0..2 {
            let sent = test_send(&app, second.id, &seeded, &seeded.cookie).await;
            assert_eq!(sent.status, 200);
        }
        let limited = test_send(&app, second.id, &seeded, &seeded.cookie).await;
        assert_eq!(limited.status, 429);
        assert!(limited.headers.get("retry-after").is_some());
        // The app sent exactly the accepted ones.
        assert_eq!(app.sent().len(), 7);
    }

    #[tokio::test]
    async fn the_daily_cap_is_spent_one_message_at_a_time() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        let channel = create_ok(
            &app,
            &seeded,
            email_body("Two", &["oncall@example.com", "backup@example.com"]),
        )
        .await;
        let now = app.state.clock.now_ns();
        assert!(
            app.state
                .alert_email_count
                .reserve(seeded.org_id, DAILY_EMAIL_LIMIT - 1, now)
        );

        // One message is left: the first recipient gets it, the second is refused at its own
        // reservation rather than the whole send being refused up front.
        let partial = test_send(&app, channel.id, &seeded, &seeded.cookie).await;
        assert_eq!(
            partial.json::<Value>(),
            json!({"ok": false, "error": "daily alert email limit reached"})
        );
        assert_eq!(app.sent().len(), 1);
    }

    #[tokio::test]
    async fn a_channel_of_another_project_is_not_found_and_spends_no_budget() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        let admin = app
            .add_member(&seeded, "admin@example.com", OrgRole::Admin)
            .await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let theirs = send(
            &app,
            Method::POST,
            &format!("/api/v1/projects/{}/channels", other.id),
            &seeded.cookie,
            Some(email_body("Theirs", &["oncall@example.com"])),
        )
        .await;
        assert_eq!(theirs.status, 201);
        let theirs = theirs.json::<NotificationChannel>();

        // The admin asks for the other project's channel through this project, five times (the
        // user's own budget): each is a 404, and none uses that channel's five sends.
        for _ in 0..5 {
            let missing = test_send(&app, theirs.id, &seeded, &admin.cookie).await;
            assert_eq!(missing.status, 404);
        }
        // The owner of that channel still has its full budget: five sends go, the sixth is 429.
        let owner_path = format!("/api/v1/projects/{}/channels/{}/test", other.id, theirs.id);
        for _ in 0..5 {
            let sent = send(&app, Method::POST, &owner_path, &seeded.cookie, None).await;
            assert_eq!(sent.status, 200);
        }
        let refused = send(&app, Method::POST, &owner_path, &seeded.cookie, None).await;
        assert_eq!(refused.status, 429);
        assert_eq!(app.sent().len(), 5);
    }

    #[tokio::test]
    async fn test_send_stops_at_the_daily_email_cap() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        let channel = create_ok(&app, &seeded, email_body("Mail", &["oncall@example.com"])).await;
        let now = app.state.clock.now_ns();
        assert!(
            app.state
                .alert_email_count
                .reserve(seeded.org_id, DAILY_EMAIL_LIMIT, now)
        );

        let refused = test_send(&app, channel.id, &seeded, &seeded.cookie).await;
        assert_eq!(refused.status, 200);
        assert_eq!(
            refused.json::<Value>(),
            json!({"ok": false, "error": "daily alert email limit reached"})
        );
        assert!(app.sent().is_empty());
    }

    #[tokio::test]
    async fn open_signup_limits_recipients_to_verified_members() {
        let app =
            TestApp::with_mailer_config(|config| config.auth.signup = SignupPolicy::Open).await;
        let seeded = app.seed().await;
        app.verify_email(seeded.user_id).await;
        let ben = app
            .add_member(&seeded, "ben@example.com", OrgRole::Member)
            .await;
        app.verify_email(ben.id).await;
        // A member whose address is not verified does not count.
        app.add_member(&seeded, "cy@example.com", OrgRole::Member)
            .await;
        let path = channels(&seeded);
        let not_members = pairs(&[(
            "recipients",
            "must be verified members of this organization",
        )]);

        let outsider = send(
            &app,
            Method::POST,
            &path,
            &seeded.cookie,
            Some(email_body("Mail", &["ada@example.com", "eve@example.com"])),
        )
        .await;
        assert_eq!(outsider.status, 400);
        assert_eq!(problem_of(&outsider), not_members);
        let unverified = send(
            &app,
            Method::POST,
            &path,
            &seeded.cookie,
            Some(email_body("Mail", &["cy@example.com"])),
        )
        .await;
        assert_eq!(unverified.status, 400);
        assert_eq!(problem_of(&unverified), not_members);

        // Verified members pass, whatever the case of the address.
        let created = create_ok(
            &app,
            &seeded,
            email_body("Mail", &["ada@example.com", "BEN@example.com"]),
        )
        .await;
        let patch = format!("{path}/{}", created.id);
        let refused = send(
            &app,
            Method::PATCH,
            &patch,
            &seeded.cookie,
            Some(json!({"recipients": ["ben@example.com", "eve@example.com"]})),
        )
        .await;
        assert_eq!(refused.status, 400);
        assert_eq!(problem_of(&refused), not_members);
        let accepted = send(
            &app,
            Method::PATCH,
            &patch,
            &seeded.cookie,
            Some(json!({"recipients": ["ben@example.com"]})),
        )
        .await;
        assert_eq!(accepted.status, 200);
    }

    #[tokio::test]
    async fn invite_only_allows_any_recipient() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        let channel = create_ok(&app, &seeded, email_body("Mail", &["eve@example.com"])).await;
        assert_eq!(channel.recipients, vec!["eve@example.com".to_owned()]);
    }

    #[tokio::test]
    async fn members_policy_limits_recipients_under_invite_only() {
        let app = TestApp::with_mailer_config(|config| {
            config.alerts.email_recipients = Some(EmailRecipients::Members);
        })
        .await;
        let seeded = app.seed().await;
        app.verify_email(seeded.user_id).await;
        let path = channels(&seeded);
        let not_members = pairs(&[(
            "recipients",
            "must be verified members of this organization",
        )]);

        let outsider = send(
            &app,
            Method::POST,
            &path,
            &seeded.cookie,
            Some(email_body("Mail", &["eve@example.com"])),
        )
        .await;
        assert_eq!(outsider.status, 400);
        assert_eq!(problem_of(&outsider), not_members);

        let created = create_ok(&app, &seeded, email_body("Mail", &["ada@example.com"])).await;
        let refused = send(
            &app,
            Method::PATCH,
            &format!("{path}/{}", created.id),
            &seeded.cookie,
            Some(json!({"recipients": ["eve@example.com"]})),
        )
        .await;
        assert_eq!(refused.status, 400);
        assert_eq!(problem_of(&refused), not_members);
    }

    #[tokio::test]
    async fn any_policy_allows_any_recipient_under_open_signup() {
        let app = TestApp::with_mailer_config(|config| {
            config.auth.signup = SignupPolicy::Open;
            config.alerts.email_recipients = Some(EmailRecipients::Any);
        })
        .await;
        let seeded = app.seed().await;
        let channel = create_ok(&app, &seeded, email_body("Mail", &["eve@example.com"])).await;
        assert_eq!(channel.recipients, vec!["eve@example.com".to_owned()]);
    }

    #[tokio::test]
    async fn members_policy_test_send_skips_a_recipient_who_left() {
        let app = TestApp::with_mailer_config(|config| {
            config.alerts.email_recipients = Some(EmailRecipients::Members);
        })
        .await;
        let seeded = app.seed().await;
        app.verify_email(seeded.user_id).await;
        let ben = app
            .add_member(&seeded, "ben@example.com", OrgRole::Member)
            .await;
        app.verify_email(ben.id).await;
        let channel = create_ok(
            &app,
            &seeded,
            email_body("Mail", &["ada@example.com", "ben@example.com"]),
        )
        .await;
        // Ben leaves the org after the channel was saved: the stored channel is checked again.
        let (org_id, ben_id) = (seeded.org_id, ben.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { remove_org_member(conn, org_id, ben_id).await })
            })
            .await
            .unwrap();

        let result = test_send(&app, channel.id, &seeded, &seeded.cookie).await;
        assert_eq!(result.status, 200);
        assert_eq!(
            result.json::<Value>(),
            json!({
                "ok": false,
                "error": "1 recipient(s) skipped: not verified members of the organization",
            })
        );
        let sent: Vec<String> = app.sent().into_iter().map(|mail| mail.to).collect();
        assert_eq!(sent, ["ada@example.com"]);
    }

    #[tokio::test]
    async fn test_channel_is_refused_in_restore_mode() {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
            config.restore_mode = true;
        })
        .await;
        let seeded = app.seed().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let channel = create_ok(
            &app,
            &seeded,
            json!({"name": "Hook", "kind": "webhook", "url": url, "secret": SECRET}),
        )
        .await;
        let path = format!("{}/{}/test", channels(&seeded), channel.id);
        let response = send(&app, Method::POST, &path, &seeded.cookie, None).await;
        crate::http::restore::tests::assert_refused(&response, "channel test");
        let reached =
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept()).await;
        assert!(reached.is_err(), "a refused test reached the channel");
    }
}
