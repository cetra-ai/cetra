// SPDX-License-Identifier: Elastic-2.0
//! The audit stream's settings API (track EA M4, D-EA-11, plan E3): one destination per org
//! (its owners manage it) and one for the instance (instance admins). Every write is recorded.
//! The URL, the signing secret and the header value are never returned: responses show the
//! host, the header's name and the delivery status only.

use axum::extract::{Extension, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cetra_core::ids::OrgId;
use cetra_eval::{EgressRequest, SIGNATURE_HEADER, Secret, sign_payload};
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::json;
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::require_instance;
use super::stream_store::{self, DestinationRow, StreamConfig, open, seal};
use crate::alerts::channel::{MAX_HEADER_VALUE_BYTES, MAX_URL_BYTES, RESERVED_HEADERS};
use crate::alerts::deliver::SEND_TIMEOUT;
use crate::api::audit::{
    AuditStream, AuditStreamStatus, AuditStreamTest, CreatedAuditStream, PutAuditStream,
    RotatedAuditStreamSecret,
};
use crate::api::entitlements::Feature;
use crate::audit::catalog::AuditAction;
use crate::audit::{Ctx, Event, TargetKind, record};
use crate::auth::roles::OrgPerm;
use crate::auth::secret::random_bytes;
use crate::crypto::SecretBox;
use crate::entitlements::require;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::http::restore::refuse_outbound;
use crate::meta::MetaError;

/// Response bytes a test send reads at most. Only the status matters.
const TEST_RESPONSE_BYTES: usize = 1024 * 1024;

/// The stream routes. Org routes need `ManageAuditStream` (owners, with a session); instance
/// routes need an instance admin. `DELETE` works without the feature, so a lapsed org can still
/// remove its destination (D-EA-19).
pub(crate) fn routes() -> Vec<RouteDef> {
    let org_access = || Access::Org {
        perm: OrgPerm::ManageAuditStream,
        session_only: true,
    };
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/orgs/{org}/audit-stream",
            access: org_access(),
            documented: true,
            handler: get(org_get),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/orgs/{org}/audit-stream",
            access: org_access(),
            documented: true,
            handler: put(org_put),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/orgs/{org}/audit-stream",
            access: org_access(),
            documented: true,
            handler: delete(org_delete),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/orgs/{org}/audit-stream/secret",
            access: org_access(),
            documented: true,
            handler: post(org_rotate),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/orgs/{org}/audit-stream/test",
            access: org_access(),
            documented: true,
            handler: post(org_test),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/admin/audit-stream",
            access: Access::InstanceAdmin,
            documented: true,
            handler: get(admin_get),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/admin/audit-stream",
            access: Access::InstanceAdmin,
            documented: true,
            handler: put(admin_put),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/admin/audit-stream",
            access: Access::InstanceAdmin,
            documented: true,
            handler: delete(admin_delete),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/admin/audit-stream/secret",
            access: Access::InstanceAdmin,
            documented: true,
            handler: post(admin_rotate),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/admin/audit-stream/test",
            access: Access::InstanceAdmin,
            documented: true,
            handler: post(admin_test),
        },
    ]
}

async fn org_get(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<AuditStream>, ApiError> {
    show(&state, Some(access.org.id)).await
}

async fn org_put(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: Ctx,
    error::Json(body): error::Json<PutAuditStream>,
) -> Result<Response, ApiError> {
    set(&state, Some(access.org.id), ctx, body).await
}

async fn org_delete(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: Ctx,
) -> Result<StatusCode, ApiError> {
    remove(&state, Some(access.org.id), ctx).await
}

async fn org_rotate(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: Ctx,
) -> Result<error::Json<RotatedAuditStreamSecret>, ApiError> {
    rotate(&state, Some(access.org.id), ctx).await
}

async fn org_test(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<AuditStreamTest>, ApiError> {
    probe(&state, Some(access.org.id)).await
}

async fn admin_get(State(state): State<AppState>) -> Result<error::Json<AuditStream>, ApiError> {
    show(&state, None).await
}

async fn admin_put(
    State(state): State<AppState>,
    ctx: Ctx,
    error::Json(body): error::Json<PutAuditStream>,
) -> Result<Response, ApiError> {
    set(&state, None, ctx, body).await
}

async fn admin_delete(State(state): State<AppState>, ctx: Ctx) -> Result<StatusCode, ApiError> {
    remove(&state, None, ctx).await
}

async fn admin_rotate(
    State(state): State<AppState>,
    ctx: Ctx,
) -> Result<error::Json<RotatedAuditStreamSecret>, ApiError> {
    rotate(&state, None, ctx).await
}

async fn admin_test(State(state): State<AppState>) -> Result<error::Json<AuditStreamTest>, ApiError> {
    probe(&state, None).await
}

/// Refuses unless the scope may stream: the org's feature, or the instance gate (plan P-5).
fn gate(state: &AppState, org: Option<OrgId>) -> Result<(), ApiError> {
    match org {
        Some(org) => require(state, org, Feature::AuditLog),
        None => require_instance(state),
    }
}

/// The 409 for a stored config that cannot be opened (the row was sealed with another key, or is
/// corrupt). Its message names no value.
fn unreadable() -> ApiError {
    ApiError::Conflict {
        detail: "The stored settings cannot be read; remove the destination and create it again.",
    }
}

/// `scope` as a record's scope: the org, or the instance (no org).
fn scoped(event: Event, org: Option<OrgId>) -> Event {
    match org {
        Some(org) => event.org(org),
        None => event,
    }
}

/// The records of `org`'s scope (`None`: the instance) after `cursor_seq`: the stream's lag.
async fn lag_of<'c, E>(executor: E, org: Option<OrgId>, cursor_seq: i64) -> Result<u64, MetaError>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE org_id IS ? AND seq > ?")
            .bind(org.map(|org| org.to_string()))
            .bind(cursor_seq)
            .fetch_one(executor)
            .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The public shape of a destination. The header's name is passed in: it is read from the sealed
/// config, and its value is never copied out.
fn stream_json(row: &DestinationRow, header_name: Option<String>, lag: u64) -> AuditStream {
    AuditStream {
        url_host: row.url_host.clone(),
        header_name,
        enabled: row.enabled,
        status: AuditStreamStatus {
            last_success_at: row.last_success_at,
            last_error: row.last_error.clone(),
            failing_since: row.failing_since,
            lag,
            lost_records: row.lost_before_seq.is_some(),
        },
    }
}

/// `GET`: the destination of `org` (`None`: the instance), or 404.
async fn show(state: &AppState, org: Option<OrgId>) -> Result<error::Json<AuditStream>, ApiError> {
    gate(state, org)?;
    let row = stream_store::destination(state.meta.reader(), org)
        .await?
        .ok_or(ApiError::NotFound)?;
    let stored = open(&state.secrets, &row.id, &row.config_enc).map_err(|_| unreadable())?;
    let lag = lag_of(state.meta.reader(), org, row.cursor_seq).await?;
    let header_name = stored.header.map(|(name, _)| name);
    Ok(error::Json(stream_json(&row, header_name, lag)))
}

/// The first problem with the endpoint URL, or its host. The messages never name the URL.
fn check_endpoint(text: &str) -> Result<String, ApiError> {
    if text.len() > MAX_URL_BYTES {
        return Err(field_error("url", "must be at most 2048 bytes"));
    }
    let url = Url::parse(text).map_err(|_| field_error("url", "must be a valid URL"))?;
    if url.scheme() != "https" {
        return Err(field_error("url", "must be an https URL"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(field_error("url", "must not contain user information"));
    }
    url.host_str()
        .map(str::to_owned)
        .ok_or_else(|| field_error("url", "must be a valid URL"))
}

/// The first problem with the header name, or `None`. Never names a value.
fn check_header_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(field_error("header_name", "must not be empty"));
    }
    if HeaderName::from_bytes(name.as_bytes()).is_err() {
        return Err(field_error("header_name", "is not a valid header name"));
    }
    if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(field_error("header_name", "is set by the server"));
    }
    Ok(())
}

/// The first problem with the header value. Never echoes the value.
fn check_header_value(value: &str) -> Result<(), ApiError> {
    if value.is_empty() {
        return Err(field_error("header_value", "must not be empty"));
    }
    if value.len() > MAX_HEADER_VALUE_BYTES {
        return Err(field_error("header_value", "must be at most 1024 bytes"));
    }
    if HeaderValue::from_str(value).is_err() {
        return Err(field_error("header_value", "is not a valid header value"));
    }
    Ok(())
}

/// The header to store, from the body (plan P-2). `stored` is the header of the stored config: a
/// body that names the same header and leaves the value out keeps the stored value.
fn header_of(
    body: &PutAuditStream,
    stored: Option<&(String, String)>,
) -> Result<Option<(String, String)>, ApiError> {
    match (body.header_name.as_deref(), body.header_value.as_deref()) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(field_error("header_value", "needs header_name")),
        (Some(name), value) => {
            check_header_name(name)?;
            let value = match value {
                Some(value) => {
                    check_header_value(value)?;
                    value.to_owned()
                }
                None => match stored {
                    Some((stored_name, stored_value)) if stored_name == name => {
                        stored_value.clone()
                    }
                    _ => return Err(field_error("header_value", "is required")),
                },
            };
            Ok(Some((name.to_owned(), value)))
        }
    }
}

/// A new signing secret: 32 random bytes, base64url without padding (plan P-1). Shown once.
fn new_secret() -> Result<String, ApiError> {
    let bytes = random_bytes(32).map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// One PUT, as its transaction needs it.
struct Upsert<'a> {
    secrets: &'a SecretBox,
    org: Option<OrgId>,
    ctx: &'a Ctx,
    body: &'a PutAuditStream,
    host: &'a str,
    now: i64,
}

/// What a PUT left: a new destination with its secret (201), or the updated one (200).
enum Saved {
    Created {
        stream: AuditStream,
        secret: String,
    },
    Updated(AuditStream),
}

/// `PUT`: creates the destination, or updates it. One transaction reads, opens, validates, seals,
/// writes and records, so two PUTs never start from the same stored config.
async fn set(
    state: &AppState,
    org: Option<OrgId>,
    ctx: Ctx,
    body: PutAuditStream,
) -> Result<Response, ApiError> {
    gate(state, org)?;
    let host = check_endpoint(&body.url)?;
    let handle = state.clone();
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let input = Upsert {
                    secrets: &handle.secrets,
                    org,
                    ctx: &ctx,
                    body: &body,
                    host: &host,
                    now,
                };
                match stream_store::destination_tx(conn, org).await? {
                    Some(row) => update(conn, &input, row).await,
                    None => create(conn, &input).await,
                }
            })
        })
        .await?;
    match outcome? {
        Saved::Created { stream, secret } => Ok((
            StatusCode::CREATED,
            error::Json(CreatedAuditStream { stream, secret }),
        )
            .into_response()),
        Saved::Updated(stream) => Ok(error::Json(stream).into_response()),
    }
}

/// Creates the destination with a new secret. The cursor starts at the table's highest `seq`
/// (plan E1), so history is not replayed.
async fn create(
    conn: &mut SqliteConnection,
    input: &Upsert<'_>,
) -> Result<Result<Saved, ApiError>, MetaError> {
    let header = match header_of(input.body, None) {
        Ok(header) => header,
        Err(error) => return Ok(Err(error)),
    };
    let secret = match new_secret() {
        Ok(secret) => secret,
        Err(error) => return Ok(Err(error)),
    };
    let id = Uuid::now_v7();
    let config = StreamConfig {
        url: input.body.url.clone(),
        secret: secret.clone(),
        header: header.clone(),
    };
    let sealed = match seal(input.secrets, &id, &config) {
        Ok(sealed) => sealed,
        Err(error) => return Ok(Err(ApiError::Internal(Box::new(error)))),
    };
    let mut row = stream_store::insert(conn, id, input.org, input.host, &sealed, input.now).await?;
    if !input.body.enabled {
        // `insert` stores the destination enabled; a disabled one is stored disabled right away.
        stream_store::update_config(conn, id, input.host, &sealed, false, input.now).await?;
        row.enabled = false;
    }
    let event = scoped(
        Event::new(AuditAction::StreamSet {
            url_host: input.host.to_owned(),
            header: header.is_some(),
            enabled: input.body.enabled,
        }),
        input.org,
    )
    .target(TargetKind::AuditStream, id);
    record(conn, input.ctx, event, input.now).await?;
    let lag = lag_of(&mut *conn, input.org, row.cursor_seq).await?;
    let stream = stream_json(&row, header.map(|(name, _)| name), lag);
    Ok(Ok(Saved::Created { stream, secret }))
}

/// Updates the stored destination `row`. Its secret is kept. The config is replaced, and the
/// failure state clears, as any edit does (plan P-6, `update_config`).
async fn update(
    conn: &mut SqliteConnection,
    input: &Upsert<'_>,
    row: DestinationRow,
) -> Result<Result<Saved, ApiError>, MetaError> {
    let stored = match open(input.secrets, &row.id, &row.config_enc) {
        Ok(stored) => stored,
        Err(_) => return Ok(Err(unreadable())),
    };
    let header = match header_of(input.body, stored.header.as_ref()) {
        Ok(header) => header,
        Err(error) => return Ok(Err(error)),
    };
    let config = StreamConfig {
        url: input.body.url.clone(),
        secret: stored.secret,
        header: header.clone(),
    };
    let sealed = match seal(input.secrets, &row.id, &config) {
        Ok(sealed) => sealed,
        Err(error) => return Ok(Err(ApiError::Internal(Box::new(error)))),
    };
    stream_store::update_config(
        conn,
        row.id,
        input.host,
        &sealed,
        input.body.enabled,
        input.now,
    )
    .await?;
    let event = scoped(
        Event::new(AuditAction::StreamSet {
            url_host: input.host.to_owned(),
            header: header.is_some(),
            enabled: input.body.enabled,
        }),
        input.org,
    )
    .target(TargetKind::AuditStream, row.id);
    record(conn, input.ctx, event, input.now).await?;
    let Some(row) = stream_store::destination_tx(conn, input.org).await? else {
        return Ok(Err(ApiError::NotFound));
    };
    let lag = lag_of(&mut *conn, input.org, row.cursor_seq).await?;
    let stream = stream_json(&row, header.map(|(name, _)| name), lag);
    Ok(Ok(Saved::Updated(stream)))
}

/// `DELETE`: removes the destination, or 404. Works without the feature (D-EA-19).
async fn remove(state: &AppState, org: Option<OrgId>, ctx: Ctx) -> Result<StatusCode, ApiError> {
    let now = state.clock.now_ns();
    let removed = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(row) = stream_store::destination_tx(conn, org).await? else {
                    return Ok(false);
                };
                stream_store::delete(conn, org).await?;
                let event = scoped(Event::new(AuditAction::StreamRemoved), org)
                    .target(TargetKind::AuditStream, row.id);
                record(conn, &ctx, event, now).await?;
                Ok(true)
            })
        })
        .await?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// `POST …/secret`: a new signing secret, shown in this response only. The URL, the header and
/// `enabled` are kept; `update_config` clears the failure state and any lost-records flag, as an
/// edit does.
async fn rotate(
    state: &AppState,
    org: Option<OrgId>,
    ctx: Ctx,
) -> Result<error::Json<RotatedAuditStreamSecret>, ApiError> {
    gate(state, org)?;
    let handle = state.clone();
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(row) = stream_store::destination_tx(conn, org).await? else {
                    return Ok(Err(ApiError::NotFound));
                };
                let mut stored = match open(&handle.secrets, &row.id, &row.config_enc) {
                    Ok(stored) => stored,
                    Err(_) => return Ok(Err(unreadable())),
                };
                let secret = match new_secret() {
                    Ok(secret) => secret,
                    Err(error) => return Ok(Err(error)),
                };
                stored.secret = secret.clone();
                let sealed = match seal(&handle.secrets, &row.id, &stored) {
                    Ok(sealed) => sealed,
                    Err(error) => return Ok(Err(ApiError::Internal(Box::new(error)))),
                };
                stream_store::update_config(
                    conn,
                    row.id,
                    &row.url_host,
                    &sealed,
                    row.enabled,
                    now,
                )
                .await?;
                let event = scoped(Event::new(AuditAction::StreamSecretRotated), org)
                    .target(TargetKind::AuditStream, row.id);
                record(conn, &ctx, event, now).await?;
                Ok(Ok(secret))
            })
        })
        .await?;
    Ok(error::Json(RotatedAuditStreamSecret {
        secret: outcome?,
    }))
}

/// `POST …/test`: one signed test line, sent now. The cursor does not move. The answer says only
/// whether the endpoint answered and with which status (plan P-7): every failure reads as not
/// reached. Refused in restore mode.
async fn probe(
    state: &AppState,
    org: Option<OrgId>,
) -> Result<error::Json<AuditStreamTest>, ApiError> {
    refuse_outbound(state, "audit stream test")?;
    gate(state, org)?;
    let row = stream_store::destination(state.meta.reader(), org)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(send_test(state, &row).await))
}

/// One test send to `row`'s endpoint.
async fn send_test(state: &AppState, row: &DestinationRow) -> AuditStreamTest {
    let not_reached = AuditStreamTest {
        reached: false,
        status: None,
    };
    let Ok(stored) = open(&state.secrets, &row.id, &row.config_enc) else {
        return not_reached;
    };
    let Ok(url) = Url::parse(&stored.url) else {
        return not_reached;
    };
    let now = state.clock.now_ns();
    let at = chrono::DateTime::from_timestamp_nanos(now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut body = serde_json::to_vec(&json!({"test": true, "at": at})).unwrap_or_default();
    body.push(b'\n');
    let Some(signature) = sign_payload(
        &Secret::new(stored.secret.clone()),
        now / 1_000_000_000,
        &body,
    ) else {
        return not_reached;
    };
    let Some(headers) = request_headers(&signature, stored.header.as_ref()) else {
        return not_reached;
    };
    let sent = state
        .eval_runner
        .egress()
        .post(EgressRequest {
            url: &url,
            headers,
            body,
            timeout: SEND_TIMEOUT,
            max_response_bytes: TEST_RESPONSE_BYTES,
        })
        .await;
    match sent {
        Ok(response) => AuditStreamTest {
            reached: true,
            status: Some(response.status.as_u16()),
        },
        Err(_) => not_reached,
    }
}

/// The request headers of a stream line: NDJSON, the signature, and the optional header. `None`
/// when a stored value is not a valid header (never happens for a row cetra wrote).
fn request_headers(
    signature: &str,
    custom: Option<&(String, String)>,
) -> Option<Vec<(HeaderName, HeaderValue)>> {
    let mut headers = vec![
        (
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson"),
        ),
        (
            HeaderName::from_static(SIGNATURE_HEADER),
            HeaderValue::from_str(signature).ok()?,
        ),
    ];
    if let Some((name, value)) = custom {
        headers.push((
            HeaderName::from_bytes(name.as_bytes()).ok()?,
            HeaderValue::from_str(value).ok()?,
        ));
    }
    Some(headers)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::api::account::OrgRole;
    use crate::api::common::ProblemCode;
    use crate::test_support::hook_receiver::HookReceiver;
    use crate::test_support::{Seeded, TestApp, TestResponse};

    const INSTANCE_PATH: &str = "/api/v1/admin/audit-stream";

    /// The app with `audit_log` licensed, and `seed()`ed.
    async fn licensed() -> (TestApp, Seeded) {
        let app = TestApp::with_license(&["audit_log"]).await;
        let seeded = app.seed().await;
        (app, seeded)
    }

    fn org_path(seeded: &Seeded) -> String {
        format!("/api/v1/orgs/{}/audit-stream", seeded.org_id)
    }

    fn put_body(header_name: Option<&str>, header_value: Option<&str>, enabled: bool) -> Value {
        json!({
            "url": "https://siem.example.com/ingest",
            "header_name": header_name,
            "header_value": header_value,
            "enabled": enabled,
        })
    }

    async fn send(
        app: &TestApp,
        method: Method,
        path: &str,
        cookie: &str,
        body: Option<&Value>,
    ) -> TestResponse {
        let request = app
            .request(method, path)
            .header(header::COOKIE.as_str(), cookie);
        match body {
            Some(body) => request.json(body).send().await,
            None => request.send().await,
        }
    }

    /// Writes a destination of `org` (`None`: the instance) directly, for URLs the API refuses.
    async fn write_destination(app: &TestApp, org: Option<OrgId>, url: &str) {
        let id = Uuid::now_v7();
        let config = StreamConfig {
            url: url.to_owned(),
            secret: "signing-secret-value".to_owned(),
            header: None,
        };
        let sealed = seal(&app.state.secrets, &id, &config).expect("seals");
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    stream_store::insert(conn, id, org, "siem.example.com", &sealed, now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("the destination is written");
    }

    async fn audit_count(app: &TestApp, action: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = ?")
            .bind(action.to_owned())
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count")
    }

    #[tokio::test]
    async fn owner_creates_a_destination_and_sees_the_secret_once() {
        let (app, seeded) = licensed().await;
        let path = org_path(&seeded);
        let created = send(
            &app,
            Method::PUT,
            &path,
            &seeded.cookie,
            Some(&put_body(Some("Authorization"), Some("Splunk token-value"), true)),
        )
        .await;
        assert_eq!(
            created.status,
            201,
            "{}",
            String::from_utf8_lossy(&created.body)
        );
        let created: Value = created.json();
        let secret = created["secret"].as_str().expect("a secret").to_owned();
        assert_eq!(secret.len(), 43);
        assert_eq!(created["stream"]["url_host"], "siem.example.com");
        assert_eq!(created["stream"]["header_name"], "Authorization");
        assert_eq!(created["stream"]["enabled"], true);

        let fetched = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(fetched.status, 200);
        let text = String::from_utf8_lossy(&fetched.body).to_string();
        assert!(!text.contains(&secret), "{text}");
        assert!(!text.contains("token-value"), "{text}");
        let fetched: Value = fetched.json();
        assert!(fetched.get("secret").is_none());

        // An update that leaves the value out keeps the stored value and shows no secret.
        let updated = send(
            &app,
            Method::PUT,
            &path,
            &seeded.cookie,
            Some(&put_body(Some("Authorization"), None, true)),
        )
        .await;
        assert_eq!(updated.status, 200, "{}", String::from_utf8_lossy(&updated.body));
        let updated: Value = updated.json();
        assert!(updated.get("secret").is_none());
        assert_eq!(updated["header_name"], "Authorization");
    }

    #[tokio::test]
    async fn admins_cannot_manage_the_stream() {
        let (app, seeded) = licensed().await;
        let admin = app
            .add_member(&seeded, "admin@acme.test", OrgRole::Admin)
            .await;
        let path = org_path(&seeded);
        let put = put_body(None, None, true);
        assert_eq!(
            send(&app, Method::GET, &path, &admin.cookie, None).await.status,
            403
        );
        assert_eq!(
            send(&app, Method::PUT, &path, &admin.cookie, Some(&put))
                .await
                .status,
            403
        );
        assert_eq!(
            send(&app, Method::DELETE, &path, &admin.cookie, None)
                .await
                .status,
            403
        );
    }

    #[tokio::test]
    async fn http_urls_and_reserved_headers_are_refused() {
        let (app, seeded) = licensed().await;
        let path = org_path(&seeded);
        let cases = [
            (
                json!({"url": "http://siem.example.com/ingest", "enabled": true}),
                "url",
            ),
            (
                json!({"url": "https://siem.example.com/ingest", "header_name": "Content-Type", "header_value": "x", "enabled": true}),
                "header_name",
            ),
            (
                json!({"url": "https://siem.example.com/ingest", "header_name": "X-Cetra-Signature", "header_value": "x", "enabled": true}),
                "header_name",
            ),
            (
                json!({"url": "https://siem.example.com/ingest", "header_value": "x", "enabled": true}),
                "header_value",
            ),
        ];
        for (put, field) in cases {
            let response = send(&app, Method::PUT, &path, &seeded.cookie, Some(&put)).await;
            assert_eq!(response.status, 400, "{field}");
            let problem: Value = response.json();
            assert_eq!(problem["errors"][0]["field"], field);
        }
    }

    #[tokio::test]
    async fn a_newline_in_the_header_value_is_refused() {
        let (app, seeded) = licensed().await;
        let put = put_body(Some("Authorization"), Some("Splunk a\nb"), true);
        let response = send(
            &app,
            Method::PUT,
            &org_path(&seeded),
            &seeded.cookie,
            Some(&put),
        )
        .await;
        assert_eq!(response.status, 400);
        let problem: Value = response.json();
        assert_eq!(problem["errors"][0]["field"], "header_value");
        assert!(!String::from_utf8_lossy(&response.body).contains("Splunk"));
    }

    #[tokio::test]
    async fn userinfo_in_the_url_is_refused() {
        let (app, seeded) = licensed().await;
        let put = json!({"url": "https://user:pw-marker@siem.example.com/ingest", "enabled": true});
        let response = send(
            &app,
            Method::PUT,
            &org_path(&seeded),
            &seeded.cookie,
            Some(&put),
        )
        .await;
        assert_eq!(response.status, 400);
        let problem: Value = response.json();
        assert_eq!(problem["errors"][0]["field"], "url");
        assert!(!String::from_utf8_lossy(&response.body).contains("pw-marker"));
    }

    #[tokio::test]
    async fn the_header_value_is_never_returned() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let put = put_body(Some("Authorization"), Some("instance-header-marker"), true);
        let created = send(&app, Method::PUT, INSTANCE_PATH, &seeded.cookie, Some(&put)).await;
        assert_eq!(created.status, 201, "{}", String::from_utf8_lossy(&created.body));
        assert!(!String::from_utf8_lossy(&created.body).contains("instance-header-marker"));
        let fetched = send(&app, Method::GET, INSTANCE_PATH, &seeded.cookie, None).await;
        assert_eq!(fetched.status, 200);
        assert!(!String::from_utf8_lossy(&fetched.body).contains("instance-header-marker"));
        let fetched: Value = fetched.json();
        assert_eq!(fetched["header_name"], "Authorization");
    }

    #[tokio::test]
    async fn rotating_the_secret_records_it() {
        let (app, seeded) = licensed().await;
        let path = org_path(&seeded);
        let created = send(
            &app,
            Method::PUT,
            &path,
            &seeded.cookie,
            Some(&put_body(None, None, true)),
        )
        .await;
        assert_eq!(created.status, 201);
        let created: Value = created.json();
        let first = created["secret"].as_str().expect("a secret").to_owned();

        let rotated = send(
            &app,
            Method::POST,
            &format!("{path}/secret"),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(rotated.status, 200, "{}", String::from_utf8_lossy(&rotated.body));
        let rotated: Value = rotated.json();
        let second = rotated["secret"].as_str().expect("a secret");
        assert_eq!(second.len(), 43);
        assert_ne!(first, second);

        let recorded: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'audit.stream_secret_rotated' AND org_id = ?",
        )
        .bind(seeded.org_id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("count");
        assert_eq!(recorded, 1);
    }

    #[tokio::test]
    async fn test_send_reports_reachability_only() {
        // A local receiver answers 202. The API refuses plain http, so the row is written directly.
        let receiver = HookReceiver::start(vec![202]).await;
        let app = TestApp::with_config(|config| config.security.allow_private_network_egress = true)
            .await;
        let seeded = app.seed().await;
        write_destination(&app, None, &receiver.url("/ingest")).await;
        let response = send(
            &app,
            Method::POST,
            &format!("{INSTANCE_PATH}/test"),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(response.status, 200, "{}", String::from_utf8_lossy(&response.body));
        assert_eq!(response.json::<Value>(), json!({"reached": true, "status": 202}));
        assert_eq!(receiver.received().len(), 1);

        // A closed port is not reached, and no status is given.
        let app = TestApp::with_config(|config| config.security.allow_private_network_egress = true)
            .await;
        let seeded = app.seed().await;
        write_destination(&app, None, "http://127.0.0.1:1/ingest").await;
        let response = send(
            &app,
            Method::POST,
            &format!("{INSTANCE_PATH}/test"),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.json::<Value>(),
            json!({"reached": false, "status": null})
        );
    }

    #[tokio::test]
    async fn delete_works_when_lapsed() {
        // No licence: the org's feature is off, and removing the destination still works.
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        write_destination(&app, Some(seeded.org_id), "https://siem.example.com/ingest").await;
        let path = org_path(&seeded);
        assert_eq!(
            send(&app, Method::PUT, &path, &seeded.cookie, Some(&put_body(None, None, true)))
                .await
                .status,
            403
        );
        assert_eq!(
            send(&app, Method::DELETE, &path, &seeded.cookie, None)
                .await
                .status,
            204
        );
        assert_eq!(
            send(&app, Method::DELETE, &path, &seeded.cookie, None)
                .await
                .status,
            404
        );
    }

    #[tokio::test]
    async fn put_needs_the_feature() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let path = org_path(&seeded);
        let refused = send(
            &app,
            Method::PUT,
            &path,
            &seeded.cookie,
            Some(&put_body(None, None, true)),
        )
        .await;
        assert_eq!(refused.status, 403);
        assert_eq!(refused.problem().code, ProblemCode::PlanRequired);
        assert_eq!(
            send(&app, Method::GET, &path, &seeded.cookie, None)
                .await
                .status,
            403
        );
        assert_eq!(
            send(&app, Method::POST, &format!("{path}/secret"), &seeded.cookie, None)
                .await
                .status,
            403
        );
    }

    #[tokio::test]
    async fn instance_routes_need_an_instance_admin_and_the_licence() {
        // A self-hosted instance without a licence: the instance gate is closed.
        let locked = TestApp::with_config(|config| config.entitlements.control_plane = false).await;
        let admin = locked.seed().await;
        let refused = send(
            &locked,
            Method::PUT,
            INSTANCE_PATH,
            &admin.cookie,
            Some(&put_body(None, None, true)),
        )
        .await;
        assert_eq!(refused.status, 403);
        assert_eq!(refused.problem().code, ProblemCode::PlanRequired);
        let member = locked
            .add_member(&admin, "member@acme.test", OrgRole::Member)
            .await;
        assert_eq!(
            send(&locked, Method::GET, INSTANCE_PATH, &member.cookie, None)
                .await
                .status,
            403
        );

        // Licensed: an instance admin creates the instance destination, disabled.
        let licensed = TestApp::with_license(&["audit_log"]).await;
        let seeded = licensed.seed().await;
        let created = send(
            &licensed,
            Method::PUT,
            INSTANCE_PATH,
            &seeded.cookie,
            Some(&put_body(None, None, false)),
        )
        .await;
        assert_eq!(created.status, 201, "{}", String::from_utf8_lossy(&created.body));
        let created: Value = created.json();
        assert_eq!(created["stream"]["enabled"], false);
    }

    #[tokio::test]
    async fn writes_are_recorded() {
        let (app, seeded) = licensed().await;
        let path = org_path(&seeded);
        let put = put_body(Some("Authorization"), Some("Splunk token-value"), true);
        assert_eq!(
            send(&app, Method::PUT, &path, &seeded.cookie, Some(&put))
                .await
                .status,
            201
        );
        assert_eq!(
            send(&app, Method::PUT, &path, &seeded.cookie, Some(&put))
                .await
                .status,
            200
        );
        assert_eq!(
            send(&app, Method::DELETE, &path, &seeded.cookie, None)
                .await
                .status,
            204
        );

        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_events WHERE org_id = ? AND action LIKE 'audit.stream%' ORDER BY seq",
        )
        .bind(seeded.org_id.to_string())
        .fetch_all(app.state.meta.reader())
        .await
        .expect("rows");
        assert_eq!(
            actions,
            vec!["audit.stream_set", "audit.stream_set", "audit.stream_removed"]
        );

        // The records name the host and the header's presence, never the URL path or the value.
        let details: Vec<String> = sqlx::query_scalar(
            "SELECT details FROM audit_events WHERE org_id = ? AND action LIKE 'audit.stream%'",
        )
        .bind(seeded.org_id.to_string())
        .fetch_all(app.state.meta.reader())
        .await
        .expect("rows");
        for details in details {
            assert!(!details.contains("token-value"), "{details}");
            assert!(!details.contains("/ingest"), "{details}");
        }
        assert_eq!(audit_count(&app, "audit.stream_set").await, 2);
    }
}
