//! LLM connection routes (spec 08, spec 09, D-I-4, D-I-24).
//!
//! Every org member reads the org's connections; admins and owners also
//! write, edit and delete them. The API key and the header values are
//! write-only: they are sealed under the row's id and never leave the
//! server again (D-I-2).
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::routing::{delete, get, patch, post};
use cetra_core::ids::OrgId;
use serde::Deserialize;
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event as CatalogEvent;
use crate::analytics::record;
use crate::api::common::{FieldError, Page, PageParams};
use crate::api::connections::{
    ConnectionStatus, CreateLlmConnectionRequest, LlmAdapter, LlmConnection,
    LlmConnectionTestError, LlmConnectionTestResult, StructuredOutputMode, TestErrorClass,
    TestLlmConnectionRequest, TestUnsavedLlmConnectionRequest, UpdateLlmConnectionRequest,
};
use crate::audit::catalog::{LlmConnectionAction, OnlineRuleAction};
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::OrgPerm;
use crate::crypto::{Aad, SecretBox};
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;
use crate::meta::MetaError;
use crate::meta::connections::{self, ConnectionFields, ConnectionRow};
use crate::meta::evaluators;
use crate::meta::issues;
use crate::meta::online_rules as rules;

use super::orgs::page_named;

/// Why a `base_url` was refused, in one sentence (D-I-4).
const BASE_URL_MESSAGE: &str = "base_url must be an http or https URL without credentials, fragment or query other than api-version";
/// At most this many extra headers (D-I-4).
const MAX_EXTRA_HEADERS: usize = 20;
/// An unsaved connection has no default model (D-I-53).
const UNSAVED_MODEL_MESSAGE: &str = "model is required to test an unsaved connection";
/// The refusal text of an embedding test on Anthropic (D-IC-5).
const NO_EMBEDDINGS_MESSAGE: &str = "This provider has no embeddings API.";
/// The prompt of a connection test's one boolean judge call (D-I-5).
const TEST_PROMPT: &str = "Reply true.";

/// LLM connection routes. Reads need [`OrgPerm::View`]; writes and tests need
/// [`OrgPerm::ManageConnections`] (D-I-24). Not session-only: a PAT works.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/orgs/{org}/llm-connections",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_llm_connections),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/orgs/{org}/llm-connections",
            access: Access::Org {
                perm: OrgPerm::ManageConnections,
                session_only: false,
            },
            documented: true,
            handler: post(create_llm_connection),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/orgs/{org}/llm-connections/{id}",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_llm_connection),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/orgs/{org}/llm-connections/{id}",
            access: Access::Org {
                perm: OrgPerm::ManageConnections,
                session_only: false,
            },
            documented: true,
            handler: patch(update_llm_connection),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/v1/orgs/{org}/llm-connections/{id}",
            access: Access::Org {
                perm: OrgPerm::ManageConnections,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_llm_connection),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/orgs/{org}/llm-connections/{id}/test",
            access: Access::Org {
                perm: OrgPerm::ManageConnections,
                session_only: false,
            },
            documented: true,
            handler: post(test_connection),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/orgs/{org}/llm-connections/test",
            access: Access::Org {
                perm: OrgPerm::ManageConnections,
                session_only: false,
            },
            documented: true,
            handler: post(test_unsaved_connection),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct IdPath {
    id: Uuid,
}

async fn list_llm_connections(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<LlmConnection>>, ApiError> {
    let rows = connections::list_connections(state.meta.reader(), access.org.id).await?;
    let items = rows
        .iter()
        .map(|row| Ok((row.name.clone(), row.id.to_string(), connection_json(row)?)))
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(error::Json(page_named(
        items,
        params.cursor.as_deref(),
        params.limit.get(),
    )?))
}

async fn get_llm_connection(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<LlmConnection>, ApiError> {
    let row = connections::get_connection(state.meta.reader(), access.org.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(connection_json(&row)?))
}

async fn create_llm_connection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateLlmConnectionRequest>,
) -> Result<(StatusCode, error::Json<LlmConnection>), ApiError> {
    let user = principal::session_user(&principal)?;
    let catalog_adapter = body.adapter;
    let mut errors = Vec::new();
    check_name(&body.name, &mut errors);
    check_api_key(&body.api_key, &mut errors);
    check_base_url(body.base_url.as_deref(), &mut errors);
    check_default_model(body.default_model.as_deref(), &mut errors);
    if let Some(value) = body.max_concurrency {
        check_max_concurrency(value, &mut errors);
    }
    if let Some(value) = body.requests_per_minute {
        check_requests_per_minute(value, &mut errors);
    }
    check_extra_headers(&body.extra_headers, &mut errors);
    if !errors.is_empty() {
        return Err(ApiError::BadRequest {
            detail: None,
            errors,
        });
    }
    let id = Uuid::now_v7();
    let (secret_enc, api_key_hint) = sealed_key(&state.secrets, &id, &body.api_key)?;
    let (extra_headers_enc, extra_header_names) =
        sealed_headers(&state.secrets, &id, &body.extra_headers)?;
    let fields = ConnectionFields {
        name: body.name.trim().to_owned(),
        adapter: body.adapter.as_str().to_owned(),
        base_url: body
            .base_url
            .as_deref()
            .map(str::trim)
            .and_then(empty_to_none),
        secret_enc,
        api_key_hint,
        extra_headers_enc,
        extra_header_names,
        default_model: body
            .default_model
            .as_deref()
            .map(str::trim)
            .and_then(empty_to_none),
        structured_output: body
            .structured_output
            .unwrap_or(StructuredOutputMode::Native)
            .as_str()
            .to_owned(),
        max_concurrency: body.max_concurrency.unwrap_or(4),
        requests_per_minute: body.requests_per_minute.unwrap_or(300),
    };
    let org_id = access.org.id;
    let user_id = user.id;
    let adapter = body.adapter.as_str();
    let now = state.clock.now_ns();
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let row =
                    connections::insert_connection(conn, id, org_id, fields, Some(user_id), now)
                        .await?;
                let event = Event::new(LlmConnectionAction::Created { adapter })
                    .org(org_id)
                    .target(TargetKind::LlmConnection, id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(row)
            })
        })
        .await?;
    record(
        &state,
        &CatalogEvent::LlmConnectionCreated {
            adapter: catalog_adapter,
        },
        &Actor::org(&principal, &access),
    );
    Ok((StatusCode::CREATED, error::Json(connection_json(&row)?)))
}

async fn update_llm_connection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<UpdateLlmConnectionRequest>,
) -> Result<error::Json<LlmConnection>, ApiError> {
    principal::session_user(&principal)?;
    let mut errors = Vec::new();
    if let Some(name) = body.name.as_deref() {
        check_name(name, &mut errors);
    }
    if let Some(key) = body.api_key.as_deref() {
        check_api_key(key, &mut errors);
    }
    check_base_url(body.base_url.as_deref(), &mut errors);
    check_default_model(body.default_model.as_deref(), &mut errors);
    if let Some(value) = body.max_concurrency {
        check_max_concurrency(value, &mut errors);
    }
    if let Some(value) = body.requests_per_minute {
        check_requests_per_minute(value, &mut errors);
    }
    if let Some(headers) = body.extra_headers.as_ref() {
        check_extra_headers(headers, &mut errors);
    }
    if !errors.is_empty() {
        return Err(ApiError::BadRequest {
            detail: None,
            errors,
        });
    }
    // The JSON names of the body's `Some` fields (plan P-2), taken before the body is
    // destructured: names only, never values.
    let changed_fields = [
        body.name.is_some().then_some("name"),
        body.adapter.is_some().then_some("adapter"),
        body.api_key.is_some().then_some("api_key"),
        body.base_url.is_some().then_some("base_url"),
        body.default_model.is_some().then_some("default_model"),
        body.structured_output
            .is_some()
            .then_some("structured_output"),
        body.max_concurrency.is_some().then_some("max_concurrency"),
        body.requests_per_minute
            .is_some()
            .then_some("requests_per_minute"),
        body.extra_headers.is_some().then_some("extra_headers"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<&'static str>>();
    let UpdateLlmConnectionRequest {
        name,
        adapter,
        api_key,
        base_url,
        default_model,
        structured_output,
        max_concurrency,
        requests_per_minute,
        extra_headers,
    } = body;
    // Sealed before the transaction: the write closure must be `Send` and
    // cannot borrow the SecretBox.
    let key = match api_key.as_deref() {
        Some(key) => Some(sealed_key(&state.secrets, &id, key)?),
        None => None,
    };
    let headers = match extra_headers.as_ref() {
        Some(headers) => Some(sealed_headers(&state.secrets, &id, headers)?),
        None => None,
    };
    let org_id = access.org.id;
    let now = state.clock.now_ns();
    let key_sent = key.is_some();
    let headers_sent = headers.is_some();
    let updated = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // Read, merge and write in one transaction, so two PATCHes
                // cannot undo each other.
                let Some(row) = connections::get_connection_tx(conn, org_id, id).await? else {
                    return Ok(None);
                };
                let fields = ConnectionFields {
                    name: name
                        .map(|value| value.trim().to_owned())
                        .unwrap_or_else(|| row.name.clone()),
                    adapter: adapter
                        .map(|value| value.as_str().to_owned())
                        .unwrap_or_else(|| row.adapter.clone()),
                    base_url: base_url
                        .map(|value| empty_to_none(value.trim()))
                        .unwrap_or_else(|| row.base_url.clone()),
                    secret_enc: key
                        .as_ref()
                        .map(|(sealed, _)| sealed.clone())
                        .unwrap_or_else(|| row.secret_enc.clone()),
                    api_key_hint: key
                        .as_ref()
                        .map(|(_, hint)| hint.clone())
                        .unwrap_or_else(|| row.api_key_hint.clone()),
                    extra_headers_enc: headers
                        .as_ref()
                        .map(|(sealed, _)| sealed.clone())
                        .unwrap_or_else(|| row.extra_headers_enc.clone()),
                    extra_header_names: headers
                        .as_ref()
                        .map(|(_, names)| names.clone())
                        .unwrap_or_else(|| row.extra_header_names.clone()),
                    default_model: default_model
                        .map(|value| empty_to_none(value.trim()))
                        .unwrap_or_else(|| row.default_model.clone()),
                    structured_output: structured_output
                        .map(|value| value.as_str().to_owned())
                        .unwrap_or_else(|| row.structured_output.clone()),
                    max_concurrency: max_concurrency.unwrap_or(row.max_concurrency),
                    requests_per_minute: requests_per_minute.unwrap_or(row.requests_per_minute),
                };
                // The stored test no longer describes what a test would call (D-I-54).
                let clears_test = key_sent
                    || headers_sent
                    || fields.adapter != row.adapter
                    || fields.base_url != row.base_url
                    || fields.structured_output != row.structured_output;
                if clears_test {
                    connections::clear_connection_test(conn, id).await?;
                }
                let Some(row) =
                    connections::update_connection(conn, org_id, id, fields, now).await?
                else {
                    return Ok(None);
                };
                let event = Event::new(LlmConnectionAction::Updated {
                    fields: changed_fields,
                    key_changed: key_sent,
                })
                .org(org_id)
                .target(TargetKind::LlmConnection, id);
                audit::record(conn, &ctx, event, now).await?;
                if !key_sent {
                    return Ok(Some((row, Vec::new(), Vec::new(), Vec::new())));
                }
                // A new key clears the status and the connection's pauses, rules and issue
                // sources alike (D-I-5, D-I-14).
                connections::set_connection_status(conn, id, "ok", None, now).await?;
                let resumed = rules::resume_connection_pauses(conn, id, now).await?;
                record_connection_resumes(conn, &ctx, &resumed, now).await?;
                let sources = issues::resume_connection_pauses(conn, id).await?;
                let using = rules::rule_ids_of_connection(conn, id).await?;
                Ok(Some((row, resumed, using, sources)))
            })
        })
        .await?;
    let Some((row, resumed, using, sources)) = updated else {
        return Err(ApiError::NotFound);
    };
    state.connections.forget(id);
    after_connection_write(
        &state,
        id,
        &using,
        &resumed,
        &sources,
        "connection key changed",
    );
    Ok(error::Json(connection_json(&row)?))
}

async fn delete_llm_connection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<StatusCode, ApiError> {
    principal::session_user(&principal)?;
    let org_id = access.org.id;
    let now = state.clock.now_ns();
    let went = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // Another org's id is 404 before the in-use check, which is not org-bound.
                if connections::get_connection_tx(conn, org_id, id)
                    .await?
                    .is_none()
                {
                    return Ok(false);
                }
                // An evaluator that uses the connection keeps it (D-I-19).
                if evaluators::connection_in_use(conn, id).await? {
                    return Err(MetaError::Conflict(
                        "connection is used by evaluators or issue sources",
                    ));
                }
                let went = connections::delete_connection(conn, org_id, id).await?;
                if went {
                    let event = Event::new(LlmConnectionAction::Deleted)
                        .org(org_id)
                        .target(TargetKind::LlmConnection, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(went)
            })
        })
        .await?;
    if !went {
        return Err(ApiError::NotFound);
    }
    state.connections.forget(id);
    Ok(StatusCode::NO_CONTENT)
}

/// Runs one boolean judge call through the connection's decrypted key and
/// answers whether it reached the provider (D-I-5). The judge trace is
/// discarded: a connection belongs to no project.
///
/// The draft fields replace the saved values for this call only (D-I-53). A
/// draft that differs from the saved values, or that sends a key or headers,
/// stores nothing.
///
/// The answers that need no call come before the saved key is opened: an
/// embedding test on Anthropic (D-IC-5) and a test with no model (400). A key
/// that no longer decrypts still gets them. A result that the provider call
/// produced is stored only while the saved connection is unchanged (D-I-54).
async fn test_connection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<TestLlmConnectionRequest>,
) -> Result<error::Json<LlmConnectionTestResult>, ApiError> {
    refuse_outbound(&state, "connection test")?;
    let user = principal::session_user(&principal)?;
    let TestLlmConnectionRequest {
        model,
        embedding_model,
        adapter,
        api_key,
        base_url,
        structured_output,
        extra_headers,
    } = body;
    let draft = DraftFields {
        adapter,
        api_key,
        base_url,
        structured_output,
        extra_headers,
    };
    // Body validation answers before the rate limiter, so an invalid body spends no test. The
    // unknown id (404) and a missing model (400) come after the limiter, so they do spend one.
    let mut errors = Vec::new();
    draft.check(&mut errors);
    if !errors.is_empty() {
        return Err(ApiError::BadRequest {
            detail: None,
            errors,
        });
    }
    state
        .limits
        .connection_tests
        .check(&user.id, state.clock.now_ns())
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let row = connections::get_connection(state.meta.reader(), access.org.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let is_draft = draft.differs_from(&row);
    let anthropic = draft
        .adapter
        .map_or(row.adapter == LlmAdapter::Anthropic.as_str(), |adapter| {
            adapter == LlmAdapter::Anthropic
        });
    let embedding_model = trimmed_model(embedding_model.as_deref());
    // A typed model equal to the default is the default model (D-I-54).
    let explicit = trimmed_model(model.as_deref())
        .filter(|model| row.default_model.as_deref() != Some(model.as_str()));
    if let Some(embedding_model) = embedding_model.clone().filter(|_| anthropic) {
        // Refused without a call, so nothing is stored (D-IC-5).
        let result = anthropic_embedding_refusal(embedding_model);
        audit_test(id, access.org.id, user.id, &result, is_draft);
        return Ok(error::Json(result));
    }
    let call = match embedding_model {
        Some(embedding_model) => TestCall::Embedding(embedding_model),
        None => TestCall::Chat(
            explicit
                .clone()
                .or_else(|| row.default_model.clone())
                .ok_or_else(|| ApiError::BadRequest {
                    detail: None,
                    errors: vec![field_error(
                        "model",
                        "model is required when the connection has no default model",
                    )],
                })?,
        ),
    };
    let saved = state
        .connections
        .get(&state.secrets, &row)
        .map_err(internal)?;
    // The cache only ever holds saved values: a draft gets its own copy.
    let connection: Arc<cetra_eval::JudgeConnection> = if is_draft {
        Arc::new(draft.apply((*saved).clone())?)
    } else {
        saved
    };
    let model = match call {
        TestCall::Embedding(embedding_model) => {
            let result =
                run_embedding_test(&state, access.org.id, &connection, embedding_model).await?;
            let now = state.clock.now_ns();
            if !is_draft {
                if !still_saved(&state, access.org.id, id, row.updated_at).await {
                    tracing::debug!(
                        connection_id = %id,
                        "connection changed during the test; result not stored"
                    );
                } else {
                    if result.ok {
                        let set_ok = row.status != "ok";
                        match recover_connection(&state, id, set_ok, false, true, now, &ctx).await {
                            Ok((rules, sources)) => {
                                after_connection_write(
                                    &state,
                                    id,
                                    &rules,
                                    &rules,
                                    &sources,
                                    "embedding test",
                                );
                            }
                            Err(error) => tracing::warn!(
                                connection_id = %id,
                                error_kind = meta_error_kind(&error),
                                "could not store the connection status"
                            ),
                        }
                    }
                    record_test(&state, id, row.updated_at, result.ok, now).await;
                }
            }
            audit_test(id, access.org.id, user.id, &result, is_draft);
            return Ok(error::Json(result));
        }
        TestCall::Chat(model) => model,
    };
    let (result, pause_class) =
        run_chat_test(&state, access.org.id, &connection, row.id, model).await?;
    let now = state.clock.now_ns();
    if !is_draft {
        if !still_saved(&state, access.org.id, id, row.updated_at).await {
            tracing::debug!(
                connection_id = %id,
                "connection changed during the test; result not stored"
            );
        } else {
            // Best effort: the provider was called, so a failed status write must not
            // lose the test result.
            let status_write = if result.ok {
                // One write sets `ok` and, on the default model, resumes the `'connection'` rule and
                // source pauses (D-I-5, D-I-14). A typed model proves the key only.
                let default_model = explicit.is_none();
                recover_connection(
                    &state,
                    id,
                    row.status != "ok",
                    default_model,
                    default_model,
                    now,
                    &ctx,
                )
                .await
                .map(|(rules, sources)| {
                    after_connection_write(&state, id, &rules, &rules, &sources, "connection test");
                })
            } else if pause_class && explicit.is_none() {
                // Only a pause on the connection's own default model marks it failing:
                // a 404 for a typed model name says nothing about the key.
                let message = result
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_default();
                set_status(&state, id, "failing", Some(message), now).await
            } else {
                Ok(())
            };
            if let Err(error) = status_write {
                // The kind only: the text can quote database state, and never carries
                // anything the provider said.
                tracing::warn!(
                    connection_id = %row.id,
                    error_kind = meta_error_kind(&error),
                    "could not store the connection status"
                );
            }
            // Only a test of the default model describes the connection (D-I-54).
            if explicit.is_none() {
                record_test(&state, id, row.updated_at, result.ok, now).await;
            }
        }
    }
    audit_test(id, access.org.id, user.id, &result, is_draft);
    Ok(error::Json(result))
}

/// What a connection test calls: an embedding of the model, or a boolean judge of the chat model.
enum TestCall {
    Chat(String),
    Embedding(String),
}

/// Whether the saved connection `id` still has `tested_updated_at`, the `updated_at` its test
/// started from (D-I-54). A save during the provider call makes the result stale: it describes
/// values that are no longer saved, so it must not set the status, the pauses or the last test. A
/// failed reload counts as changed, and is logged by its kind.
async fn still_saved(state: &AppState, org_id: OrgId, id: Uuid, tested_updated_at: i64) -> bool {
    match connections::get_connection(state.meta.reader(), org_id, id).await {
        Ok(Some(current)) => current.updated_at == tested_updated_at,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                connection_id = %id,
                error_kind = meta_error_kind(&error),
                "could not reload the connection"
            );
            false
        }
    }
}

/// The audit line of a connection test (D-I-5). It carries no payload and no key.
fn audit_test(
    id: Uuid,
    org_id: OrgId,
    user_id: Uuid,
    result: &LlmConnectionTestResult,
    draft: bool,
) {
    tracing::info!(
        connection_id = %id,
        org_id = %org_id,
        user_id = %user_id,
        ok = result.ok,
        latency_ms = result.latency_ms,
        draft = draft,
        "llm connection tested"
    );
}

/// Runs one boolean judge call of `model` through `connection`, as the connection test does
/// (D-I-5). Answers the result and whether a pause-class failure said the key, quota or model
/// access was refused. The judge's `connection_id` is `evaluator_id`; no row is written.
async fn run_chat_test(
    state: &AppState,
    org_id: OrgId,
    connection: &cetra_eval::JudgeConnection,
    evaluator_id: Uuid,
    model: String,
) -> Result<(LlmConnectionTestResult, bool), ApiError> {
    let evaluator = cetra_eval::Evaluator {
        id: evaluator_id,
        name: "connection test".to_owned(),
        config: cetra_eval::EvaluatorConfig::LlmJudge(cetra_eval::JudgeConfig {
            connection_id: evaluator_id,
            model: model.clone(),
            prompt: TEST_PROMPT.to_owned(),
            output: cetra_eval::JudgeOutput::Boolean,
            include_reason: false,
            temperature: None,
            // Reasoning models spend tokens before answering; 1024 can truncate a working call.
            max_output_tokens: 4096,
            timeout_ms: 30_000,
        }),
        output: cetra_eval::OutputScore {
            name: "connection_test".to_owned(),
            data_type: cetra_core::enums::ScoreDataType::Boolean,
            categories: None,
        },
    };
    let ingest = cetra_otel::IngestContext {
        // A connection belongs to no project, and the judge trace is discarded.
        project_id: cetra_core::ids::ProjectId::from_uuid(Uuid::nil()),
        ingest_time: state.clock.now_ns(),
        retention_days: None,
        limits: state.config.ingest.limits(&state.config.media),
    };
    let prices = state.ingest.prices().for_org(org_id);
    let bytes = crate::auth::secret::random_bytes(16).map_err(internal)?;
    let bytes = <[u8; 16]>::try_from(bytes.as_slice()).map_err(internal)?;
    let judge_trace_id = cetra_core::ids::TraceId::from_bytes(bytes).map_err(internal)?; // refuses all-zero bytes only
    let started = std::time::Instant::now();
    let run = state
        .eval_runner
        .run(cetra_eval::RunInput {
            evaluator: &evaluator,
            variables: &cetra_eval::Variables::from_map(BTreeMap::new()),
            connection: Some(connection),
            webhook_secret: None,
            target: cetra_eval::EvalTarget {
                trace_id: None,
                span_id: None,
            },
            origin: cetra_eval::EvalOrigin::Test,
            judge_trace_id,
            ingest: &ingest,
            prices: &prices,
        })
        .await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut result = LlmConnectionTestResult {
        ok: true,
        model,
        latency_ms,
        error: None,
    };
    // A pause-class failure says the key, quota or model access was refused.
    let mut pause_class = false;
    if let Err(error) = run.outcome {
        pause_class = error.class() == cetra_eval::ErrorClass::Pause;
        result.ok = false;
        result.error = Some(LlmConnectionTestError {
            class: match error.class() {
                cetra_eval::ErrorClass::Retryable => TestErrorClass::Retryable,
                cetra_eval::ErrorClass::Pause => TestErrorClass::Pause,
                cetra_eval::ErrorClass::Permanent => TestErrorClass::Permanent,
            },
            // The display quotes no payload (D160).
            message: error.to_string(),
        });
    }
    Ok((result, pause_class))
}

/// Runs one boolean judge or embedding call through a connection that is not saved, and stores
/// nothing (D-I-53).
async fn test_unsaved_connection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Json(body): error::Json<TestUnsavedLlmConnectionRequest>,
) -> Result<error::Json<LlmConnectionTestResult>, ApiError> {
    refuse_outbound(&state, "connection test")?;
    let user = principal::session_user(&principal)?;
    let TestUnsavedLlmConnectionRequest {
        adapter,
        api_key,
        base_url,
        structured_output,
        extra_headers,
        model,
        embedding_model,
    } = body;
    let embedding_model = trimmed_model(embedding_model.as_deref());
    let model = trimmed_model(model.as_deref());
    let mut errors = Vec::new();
    check_api_key(&api_key, &mut errors);
    check_base_url(base_url.as_deref(), &mut errors);
    check_extra_headers(&extra_headers, &mut errors);
    if embedding_model.is_none() && model.is_none() {
        errors.push(field_error("model", UNSAVED_MODEL_MESSAGE));
    }
    if !errors.is_empty() {
        return Err(ApiError::BadRequest {
            detail: None,
            errors,
        });
    }
    state
        .limits
        .connection_tests
        .check(&user.id, state.clock.now_ns())
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let base = cetra_eval::JudgeConnection {
        adapter: eval_adapter(adapter)?,
        base_url: None,
        api_key: cetra_eval::Secret::new(api_key),
        extra_headers: Vec::new(),
        structured_output: cetra_eval::StructuredOutput::Native,
    };
    let connection = DraftFields {
        adapter: None,
        api_key: None,
        base_url,
        structured_output,
        extra_headers: Some(extra_headers),
    }
    .apply(base)?;
    let result = if let Some(embedding_model) = embedding_model {
        if adapter == LlmAdapter::Anthropic {
            anthropic_embedding_refusal(embedding_model)
        } else {
            run_embedding_test(&state, access.org.id, &connection, embedding_model).await?
        }
    } else {
        let Some(model) = model else {
            return Err(unsaved_model_required());
        };
        run_chat_test(&state, access.org.id, &connection, Uuid::nil(), model)
            .await?
            .0
    };
    tracing::info!(
        org_id = %access.org.id,
        user_id = %user.id,
        ok = result.ok,
        latency_ms = result.latency_ms,
        "llm connection draft tested"
    );
    Ok(error::Json(result))
}

/// Embeds `"ping"` with `model` through `connection` (D-IC-5) and answers the result. The call
/// trace is discarded, as the judge's. Nothing is written: the caller decides what the result
/// means for the stored connection.
///
/// # Errors
///
/// Returns [`ApiError`] when the call's random trace id cannot be made; a failed provider call is
/// an `ok: false` result, not an error.
async fn run_embedding_test(
    state: &AppState,
    org_id: OrgId,
    connection: &cetra_eval::JudgeConnection,
    model: String,
) -> Result<LlmConnectionTestResult, ApiError> {
    let ingest = cetra_otel::IngestContext {
        // A connection belongs to no project, and the call trace is discarded.
        project_id: cetra_core::ids::ProjectId::from_uuid(Uuid::nil()),
        ingest_time: state.clock.now_ns(),
        retention_days: None,
        limits: state.config.ingest.limits(&state.config.media),
    };
    let prices = state.ingest.prices().for_org(org_id);
    let bytes = crate::auth::secret::random_bytes(16).map_err(internal)?;
    let bytes = <[u8; 16]>::try_from(bytes.as_slice()).map_err(internal)?;
    let trace_id = cetra_core::ids::TraceId::from_bytes(bytes).map_err(internal)?; // refuses all-zero bytes only
    let started = std::time::Instant::now();
    let run = state
        .eval_runner
        .embed(cetra_eval::EmbeddingInput {
            connection,
            model: &model,
            text: "ping",
            dimensions: None,
            timeout: std::time::Duration::from_secs(30),
            trace: cetra_eval::CallTrace {
                trace_id,
                root_name: "cetra.issue.embed",
                metadata: &[],
                ingest: &ingest,
                prices: &prices,
            },
        })
        .await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut result = LlmConnectionTestResult {
        ok: true,
        model,
        latency_ms,
        error: None,
    };
    if let Err(error) = run.outcome {
        result.ok = false;
        result.error = Some(LlmConnectionTestError {
            class: match error.class() {
                cetra_eval::ErrorClass::Retryable => TestErrorClass::Retryable,
                cetra_eval::ErrorClass::Pause => TestErrorClass::Pause,
                cetra_eval::ErrorClass::Permanent => TestErrorClass::Permanent,
            },
            // The display quotes no payload (D160).
            message: error.to_string(),
        });
    }
    Ok(result)
}

/// The draft fields of a connection test (D-I-53), with PATCH semantics: an absent field is the
/// saved value.
struct DraftFields {
    adapter: Option<LlmAdapter>,
    api_key: Option<String>,
    base_url: Option<String>,
    structured_output: Option<StructuredOutputMode>,
    extra_headers: Option<BTreeMap<String, String>>,
}

impl DraftFields {
    /// The checks of the connection write on the fields the draft sends. No rule ties
    /// `base_url` or `adapter` to `api_key`: an absent key is the saved key (Q1).
    fn check(&self, errors: &mut Vec<FieldError>) {
        if let Some(key) = self.api_key.as_deref() {
            check_api_key(key, errors);
        }
        check_base_url(self.base_url.as_deref(), errors);
        if let Some(headers) = self.extra_headers.as_ref() {
            check_extra_headers(headers, errors);
        }
    }

    /// Whether a test with these fields calls something other than the saved values, or sends a
    /// write-only value, which is never compared with the saved one.
    fn differs_from(&self, row: &ConnectionRow) -> bool {
        self.api_key.is_some()
            || self.extra_headers.is_some()
            || self
                .adapter
                .is_some_and(|adapter| adapter.as_str() != row.adapter)
            || self
                .base_url
                .as_deref()
                .is_some_and(|base| empty_to_none(base.trim()) != row.base_url)
            || self
                .structured_output
                .is_some_and(|mode| mode.as_str() != row.structured_output)
    }

    /// The connection a test calls: `base` with each draft field replacing its value.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] when a draft value is not one the call can take; `check` has
    /// already refused those.
    fn apply(
        &self,
        mut base: cetra_eval::JudgeConnection,
    ) -> Result<cetra_eval::JudgeConnection, ApiError> {
        if let Some(adapter) = self.adapter {
            base.adapter = eval_adapter(adapter)?;
        }
        if let Some(key) = self.api_key.as_ref() {
            base.api_key = cetra_eval::Secret::new(key.clone());
        }
        if let Some(value) = self.base_url.as_deref() {
            base.base_url = parsed_base_url(value)?;
        }
        if let Some(mode) = self.structured_output {
            base.structured_output = eval_structured(mode)?;
        }
        if let Some(headers) = self.extra_headers.as_ref() {
            base.extra_headers = header_secrets(headers);
        }
        Ok(base)
    }
}

/// The eval crate's adapter for an API adapter.
fn eval_adapter(adapter: LlmAdapter) -> Result<cetra_eval::Adapter, ApiError> {
    cetra_eval::Adapter::from_name(adapter.as_str()).map_err(internal)
}

/// The eval crate's mode for an API structured output mode.
fn eval_structured(mode: StructuredOutputMode) -> Result<cetra_eval::StructuredOutput, ApiError> {
    cetra_eval::StructuredOutput::from_name(mode.as_str()).map_err(internal)
}

/// A checked `base_url` as the call takes it: empty is the provider's default.
fn parsed_base_url(value: &str) -> Result<Option<url::Url>, ApiError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    url::Url::parse(trimmed)
        .map(Some)
        .map_err(|_| ApiError::BadRequest {
            detail: None,
            errors: vec![field_error("base_url", BASE_URL_MESSAGE)],
        })
}

/// Header values as the call sends them: names trimmed and lowercased, as `sealed_headers`
/// stores them.
fn header_secrets(headers: &BTreeMap<String, String>) -> Vec<(String, cetra_eval::Secret)> {
    let map: BTreeMap<String, String> = headers
        .iter()
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.clone()))
        .collect();
    map.into_iter()
        .map(|(name, value)| (name, cetra_eval::Secret::new(value)))
        .collect()
}

/// A trimmed model name; empty is absent.
fn trimmed_model(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

/// The 400 of an unsaved test that names no model.
fn unsaved_model_required() -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors: vec![field_error("model", UNSAVED_MODEL_MESSAGE)],
    }
}

/// The answer of an embedding test on Anthropic, without a call (D-IC-5).
fn anthropic_embedding_refusal(model: String) -> LlmConnectionTestResult {
    LlmConnectionTestResult {
        ok: false,
        model,
        latency_ms: 0,
        error: Some(LlmConnectionTestError {
            class: TestErrorClass::Permanent,
            message: NO_EMBEDDINGS_MESSAGE.to_owned(),
        }),
    }
}

/// Records each online rule that a connection write resumed: one `online_rule.resumed` row per
/// rule, reason `connection`, in the write's transaction (D-EA-21). A rule with no project is
/// skipped.
///
/// # Errors
///
/// Returns [`MetaError`] when a lookup or the insert fails; the caller's transaction then rolls back.
async fn record_connection_resumes(
    conn: &mut sqlx::SqliteConnection,
    ctx: &audit::Ctx,
    resumed: &[Uuid],
    now: i64,
) -> Result<(), MetaError> {
    for &rule in resumed {
        let Some((org, project)) = audit::scope_of_rule(conn, rule).await? else {
            continue;
        };
        let event = Event::new(OnlineRuleAction::Resumed {
            reason: "connection",
        })
        .org(org)
        .project(project)
        .target(TargetKind::OnlineRule, rule);
        audit::record(conn, ctx, event, now).await?;
    }
    Ok(())
}

/// Sets the connection's status to `ok` when `set_ok`, and clears its `'connection'` pauses, in one
/// write: the rules' when `resume_rules`, the issue sources' when `resume_sources`. Returns the
/// resumed rules and sources. A test on a typed chat model passes neither: it proves the key works,
/// but the pauses wait for a test on the default model (D-I-5). An embedding test resumes the
/// sources only.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
async fn recover_connection(
    state: &AppState,
    id: Uuid,
    set_ok: bool,
    resume_rules: bool,
    resume_sources: bool,
    now: i64,
    ctx: &audit::Ctx,
) -> Result<(Vec<Uuid>, Vec<Uuid>), MetaError> {
    let ctx = ctx.clone();
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if set_ok {
                    connections::set_connection_status(conn, id, "ok", None, now).await?;
                }
                let rules = if resume_rules {
                    rules::resume_connection_pauses(conn, id, now).await?
                } else {
                    Vec::new()
                };
                record_connection_resumes(conn, &ctx, &rules, now).await?;
                let sources = if resume_sources {
                    issues::resume_connection_pauses(conn, id).await?
                } else {
                    Vec::new()
                };
                Ok((rules, sources))
            })
        })
        .await
}

/// After a connection write: the breakers of `reset` start over, each resumed rule is audited,
/// and the dispatcher wakes when there is something to run (D-I-14). The issue workers wake when
/// a source was resumed.
fn after_connection_write(
    state: &AppState,
    connection: Uuid,
    reset: &[Uuid],
    resumed: &[Uuid],
    sources: &[Uuid],
    reason: &'static str,
) {
    for rule in reset {
        state.eval.breakers.reset(*rule);
    }
    for rule in resumed {
        tracing::info!(
            rule_id = %rule,
            connection_id = %connection,
            reason,
            "online rule resumed"
        );
    }
    if !reset.is_empty() {
        state.eval.wake();
    }
    if !sources.is_empty() {
        state.issues_wake();
    }
}

/// Stores a connection's new `status` without touching `updated_at` (D-I-5).
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails. The caller decides what to do
/// with it.
async fn set_status(
    state: &AppState,
    id: Uuid,
    status: &'static str,
    error: Option<String>,
    now: i64,
) -> Result<(), MetaError> {
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                connections::set_connection_status(conn, id, status, error.as_deref(), now).await
            })
        })
        .await
}

/// Stores a saved-values test's outcome (D-I-54). Best effort, as the status write: the provider
/// was called, so a failed write must not lose the test result.
async fn record_test(state: &AppState, id: Uuid, tested_updated_at: i64, ok: bool, now: i64) {
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                connections::record_connection_test(conn, id, tested_updated_at, ok, now).await
            })
        })
        .await;
    if let Err(error) = written {
        tracing::warn!(
            connection_id = %id,
            error_kind = meta_error_kind(&error),
            "could not store the connection test"
        );
    }
}

/// The kind of a [`MetaError`], for logs that carry no error text.
fn meta_error_kind(error: &MetaError) -> &'static str {
    match error {
        MetaError::Open { .. } => "open",
        MetaError::Migrate { .. } => "migrate",
        MetaError::NotInitialized => "not initialized",
        MetaError::SchemaMismatch { .. } => "schema mismatch",
        MetaError::FormatTooNew { .. } => "format too new",
        MetaError::SchemaTooNew { .. } => "schema too new",
        MetaError::PreFormat => "pre-format",
        MetaError::Db(_) => "db",
        MetaError::Conflict(_) => "conflict",
        MetaError::Corrupt { .. } => "corrupt",
        MetaError::PreMigrationIo(_) => "pre-migration io",
        MetaError::PreMigrationDb(_) => "pre-migration db",
    }
}

/// The stored connection as the API shows it, without the sealed values.
///
/// # Errors
///
/// Returns [`ApiError::Internal`] when a stored name is not one of the
/// connection's own values.
fn connection_json(row: &ConnectionRow) -> Result<LlmConnection, ApiError> {
    let corrupt = |column: &'static str| {
        ApiError::Internal(Box::new(MetaError::Corrupt {
            table: "llm_connections",
            column,
        }))
    };
    Ok(LlmConnection {
        id: row.id,
        name: row.name.clone(),
        adapter: LlmAdapter::from_name(&row.adapter).ok_or_else(|| corrupt("adapter"))?,
        base_url: row.base_url.clone(),
        default_model: row.default_model.clone(),
        structured_output: StructuredOutputMode::from_name(&row.structured_output)
            .ok_or_else(|| corrupt("structured_output"))?,
        max_concurrency: row.max_concurrency,
        requests_per_minute: row.requests_per_minute,
        api_key_hint: row.api_key_hint.clone(),
        extra_header_names: row.extra_header_names.clone(),
        status: ConnectionStatus::from_name(&row.status).ok_or_else(|| corrupt("status"))?,
        last_error: row.last_error.clone(),
        last_error_at: row.last_error_at,
        last_tested_at: row.last_tested_at,
        last_test_ok: row.last_test_ok,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Seals the API key for the row `id`, with the hint shown in its place.
fn sealed_key(secrets: &SecretBox, id: &Uuid, key: &str) -> Result<(Vec<u8>, String), ApiError> {
    let sealed = secrets
        .seal(
            key.as_bytes(),
            Aad {
                table: "llm_connections",
                column: "secret_enc",
                row_id: id,
            },
        )
        .map_err(internal)?;
    Ok((sealed, key_hint(key)))
}

/// Seals the extra headers for the row `id`, keyed by their lowercased names,
/// and lists those names in the map's order.
fn sealed_headers(
    secrets: &SecretBox,
    id: &Uuid,
    headers: &BTreeMap<String, String>,
) -> Result<(Option<Vec<u8>>, Vec<String>), ApiError> {
    if headers.is_empty() {
        return Ok((None, Vec::new()));
    }
    let map: BTreeMap<String, String> = headers
        .iter()
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.clone()))
        .collect();
    let names = map.keys().cloned().collect();
    let bytes = serde_json::to_vec(&map).map_err(internal)?;
    let sealed = secrets
        .seal(
            &bytes,
            Aad {
                table: "llm_connections",
                column: "extra_headers_enc",
                row_id: id,
            },
        )
        .map_err(internal)?;
    Ok((Some(sealed), names))
}

/// The key's last 4 characters when it has at least 16, else an empty
/// string (D-I-4).
fn key_hint(key: &str) -> String {
    let count = key.chars().count();
    if count >= 16 {
        key.chars().skip(count - 4).collect()
    } else {
        String::new()
    }
}

/// A trimmed text; an empty result stays unset.
fn empty_to_none(trimmed: &str) -> Option<String> {
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

/// `name`: 1 to 100 characters after trimming.
fn check_name(name: &str, errors: &mut Vec<FieldError>) {
    let length = name.trim().chars().count();
    if !(1..=100).contains(&length) {
        errors.push(field_error("name", "name must be 1 to 100 characters"));
    }
}

/// `api_key`: 1 to 4096 bytes, no whitespace at either end, no control
/// character.
fn check_api_key(key: &str, errors: &mut Vec<FieldError>) {
    if key.is_empty()
        || key.len() > 4096
        || key.starts_with(char::is_whitespace)
        || key.ends_with(char::is_whitespace)
        || key.chars().any(char::is_control)
    {
        errors.push(field_error(
            "api_key",
            "api_key must be 1 to 4096 bytes, without spaces at either end",
        ));
    }
}

/// `base_url` when it carries a value: no control character, and an http or
/// https URL without credentials, fragment, or a query other than one
/// `api-version` pair. An empty or whitespace-only value clears the field and
/// is not checked.
fn check_base_url(value: Option<&str>, errors: &mut Vec<FieldError>) {
    let Some(raw) = value else {
        return;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    // `Url::parse` strips tab, CR and LF, so the raw value is checked: a
    // control character must not be stored.
    let no_control = !raw.chars().any(char::is_control);
    if no_control && base_url_is_valid(trimmed) {
        return;
    }
    errors.push(field_error("base_url", BASE_URL_MESSAGE));
}

fn base_url_is_valid(value: &str) -> bool {
    if value.len() > 2048 {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    // Other query keys are refused because providers put keys there (?key=,
    // ?code=), and `base_url` is shown to every org member.
    let query_ok = match url.query() {
        None => true,
        Some(_) => {
            let pairs: Vec<_> = url.query_pairs().collect();
            pairs.len() == 1 && pairs[0].0 == "api-version"
        }
    };
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && authority_is_written(value, &url)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && query_ok
}

/// `url` forgives an empty authority (`http:///v1` parses with host `v1`),
/// so the host must also be written in the input.
fn authority_is_written(value: &str, url: &url::Url) -> bool {
    let rest = value
        .get(url.scheme().len()..)
        .and_then(|rest| rest.strip_prefix("://"))
        .unwrap_or("");
    rest.chars()
        .next()
        .is_some_and(|ch| ch != '/' && ch != '\\')
}

/// `default_model` when it carries a value: the eval crate's own model rule,
/// 1 to 200 bytes. An empty or whitespace-only value clears the field and is
/// not checked.
fn check_default_model(model: Option<&str>, errors: &mut Vec<FieldError>) {
    let Some(trimmed) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return;
    };
    let forbidden = trimmed.chars().any(|ch| {
        ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '\\' | '%' | '?' | '#')
    });
    if !(1..=200).contains(&trimmed.len()) || forbidden {
        errors.push(field_error(
            "default_model",
            "default_model must be 1 to 200 characters without spaces or / \\ % ? #",
        ));
    }
}

fn check_max_concurrency(value: u32, errors: &mut Vec<FieldError>) {
    if !(1..=64).contains(&value) {
        errors.push(field_error(
            "max_concurrency",
            "max_concurrency must be between 1 and 64",
        ));
    }
}

fn check_requests_per_minute(value: u32, errors: &mut Vec<FieldError>) {
    if !(1..=10_000).contains(&value) {
        errors.push(field_error(
            "requests_per_minute",
            "requests_per_minute must be between 1 and 10000",
        ));
    }
}

/// `extra_headers`: at most 20 entries, names HTTP accepts that are not
/// lowercased duplicates and not headers the provider connection owns, and
/// header-safe values of at most 4096 bytes.
fn check_extra_headers(headers: &BTreeMap<String, String>, errors: &mut Vec<FieldError>) {
    if headers.len() > MAX_EXTRA_HEADERS {
        errors.push(field_error("extra_headers", "at most 20 extra headers"));
        return;
    }
    let mut seen = BTreeSet::new();
    for (name, value) in headers {
        let trimmed = name.trim();
        if HeaderName::from_bytes(trimmed.as_bytes()).is_err() {
            errors.push(field_error("extra_headers", "invalid header name"));
            continue;
        }
        let lowercase = trimmed.to_ascii_lowercase();
        let owned = matches!(
            lowercase.as_str(),
            "content-type" | "content-length" | "host" | "transfer-encoding"
        );
        let duplicate = !seen.insert(lowercase.clone());
        let value_ok = value.len() <= 4096 && HeaderValue::from_str(value).is_ok();
        if owned || duplicate || !value_ok {
            errors.push(field_error(
                &format!("extra_headers.{lowercase}"),
                "invalid header",
            ));
        }
    }
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{HeaderMap, Method, header};
    use cetra_core::ids::OrgId;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::api::account::OrgRole;
    use crate::auth::session::start_session;
    use crate::meta::evaluators::EvaluatorFields;
    use crate::meta::members::add_org_member;
    use crate::meta::online_rules::{PAUSE_CONNECTION, PAUSE_ERRORS};
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::mock_llm::{MockLlm, Reply, completion, embedding};
    use crate::test_support::{TestApp, TestResponse, capture_logs};

    fn cookie_pair(set_cookie: &HeaderValue) -> String {
        set_cookie
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn signed_in(app: &TestApp, email: &str) -> (Uuid, String) {
        let email = email.to_owned();
        let now = app.state.clock.now_ns();
        let user = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(
                        conn,
                        NewUser {
                            email,
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(
                                crate::meta::users::EmailVerifiedVia::MailToken,
                            ),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let cookie = start_session(
            &app.state,
            user.id,
            None,
            None,
            crate::auth::session::SessionOrigin::Password,
        )
        .await
        .unwrap();
        (user.id, cookie_pair(&cookie))
    }

    async fn add_member(app: &TestApp, org: OrgId, user: Uuid, role: OrgRole) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { add_org_member(conn, org, user, role, now).await })
            })
            .await
            .unwrap();
    }

    /// A fresh app with org `acme` and its owner's cookie.
    async fn app() -> (TestApp, String) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        (app, seeded.cookie)
    }

    /// [`app`] with private-network egress allowed, so judges can reach the
    /// mock provider on `127.0.0.1`.
    async fn egress_app() -> (TestApp, String) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        (app, seeded.cookie)
    }

    async fn create(app: &TestApp, cookie: &str, body: &Value) -> TestResponse {
        app.request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), cookie)
            .json(body)
            .send()
            .await
    }

    async fn patch(app: &TestApp, cookie: &str, id: Uuid, body: &Value) -> TestResponse {
        app.request(
            Method::PATCH,
            &format!("/api/v1/orgs/acme/llm-connections/{id}"),
        )
        .header(header::COOKIE.as_str(), cookie)
        .json(body)
        .send()
        .await
    }

    async fn get(app: &TestApp, cookie: &str, id: Uuid) -> TestResponse {
        app.request(
            Method::GET,
            &format!("/api/v1/orgs/acme/llm-connections/{id}"),
        )
        .header(header::COOKIE.as_str(), cookie)
        .send()
        .await
    }

    async fn delete(app: &TestApp, cookie: &str, id: Uuid) -> TestResponse {
        app.request(
            Method::DELETE,
            &format!("/api/v1/orgs/acme/llm-connections/{id}"),
        )
        .header(header::COOKIE.as_str(), cookie)
        .send()
        .await
    }

    async fn test(app: &TestApp, cookie: &str, id: Uuid, body: &Value) -> TestResponse {
        app.request(
            Method::POST,
            &format!("/api/v1/orgs/acme/llm-connections/{id}/test"),
        )
        .header(header::COOKIE.as_str(), cookie)
        .json(body)
        .send()
        .await
    }

    /// A connection pointing at `base_url` with the default model `gpt-test`.
    async fn mocked(app: &TestApp, cookie: &str, mock: &MockLlm) -> Uuid {
        let created = create(
            app,
            cookie,
            &json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test"
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        created_id(&created)
    }

    /// The header value, when it is one valid string.
    fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap()
    }

    fn body(name: &str) -> Value {
        json!({ "name": name, "adapter": "openai", "api_key": "sk-test-0123456789abcdef" })
    }

    fn created_id(response: &TestResponse) -> Uuid {
        response.json::<LlmConnection>().id
    }

    /// The stored `secret_enc` of `id`.
    async fn stored_secret(app: &TestApp, id: Uuid) -> Vec<u8> {
        sqlx::query_scalar::<_, Option<Vec<u8>>>(
            "SELECT secret_enc FROM llm_connections WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
        .expect("a sealed key")
    }

    /// The stored `extra_headers_enc` of `id`; `None` when the column is NULL.
    async fn stored_headers(app: &TestApp, id: Uuid) -> Option<Vec<u8>> {
        sqlx::query_scalar::<_, Option<Vec<u8>>>(
            "SELECT extra_headers_enc FROM llm_connections WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    /// Two rules of one judge on `connection`: one paused `'connection'`, one paused `'errors'`.
    /// Returns their ids in that order.
    async fn paused_rules(
        app: &TestApp,
        seeded: &crate::test_support::Seeded,
        connection: Uuid,
    ) -> (Uuid, Uuid) {
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let judge = Uuid::now_v7();
                    evaluators::insert_evaluator(
                        conn,
                        judge,
                        project,
                        "llm_judge",
                        EvaluatorFields {
                            name: "relevance".to_owned(),
                            config: json!({
                                "connection_id": connection,
                                "model": "gpt-test",
                                "prompt": "Q {{ input }}",
                                "output": {"type": "boolean"},
                                "include_reason": false,
                                "temperature": null,
                                "max_output_tokens": 1024,
                                "timeout_ms": 60000
                            })
                            .to_string(),
                            output_score: json!({"name": "relevance", "data_type": "boolean"})
                                .to_string(),
                            connection_id: Some(connection),
                            secret_enc: None,
                            version: 1,
                        },
                        None,
                        now,
                    )
                    .await?;
                    let mut ids = Vec::new();
                    for (name, reason) in
                        [("connection", PAUSE_CONNECTION), ("errors", PAUSE_ERRORS)]
                    {
                        let id = Uuid::now_v7();
                        rules::insert_rule(
                            conn,
                            rules::NewRule {
                                id,
                                project_id: project,
                                evaluator_id: judge,
                                target: "trace".to_owned(),
                                created_by: None,
                            },
                            rules::RuleFields {
                                name: name.to_owned(),
                                filter: None,
                                variable_mapping: r#"{"input":"trace.input"}"#.to_owned(),
                                sample_rate: 1.0,
                                budget_usd: None,
                                budget_period: None,
                                enabled: true,
                            },
                            now,
                        )
                        .await?;
                        rules::pause_rule(conn, id, reason, None, now).await?;
                        ids.push(id);
                    }
                    Ok((ids[0], ids[1]))
                })
            })
            .await
            .unwrap()
    }

    async fn paused_reason_of(app: &TestApp, id: Uuid) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT paused_reason FROM online_rules WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_successful_test_resumes_connection_pauses() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        let id = mocked(&app, &seeded.cookie, &mock).await;
        let (by_connection, by_errors) = paused_rules(&app, &seeded, id).await;

        let response = test(&app, &seeded.cookie, id, &json!({})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);
        assert_eq!(paused_reason_of(&app, by_connection).await, None);
        assert_eq!(
            paused_reason_of(&app, by_errors).await.as_deref(),
            Some(PAUSE_ERRORS)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_successful_test_on_a_typed_model_keeps_connection_pauses() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        let id = mocked(&app, &seeded.cookie, &mock).await;
        let (by_connection, by_errors) = paused_rules(&app, &seeded, id).await;

        let response = test(&app, &seeded.cookie, id, &json!({"model": "gpt-other"})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);
        assert_eq!(
            paused_reason_of(&app, by_connection).await.as_deref(),
            Some(PAUSE_CONNECTION)
        );
        assert_eq!(
            paused_reason_of(&app, by_errors).await.as_deref(),
            Some(PAUSE_ERRORS)
        );
    }

    /// An issue source of `seeded`'s project that uses `connection` for embeddings, paused by it.
    /// Returns its id.
    async fn embedding_source_paused(
        app: &TestApp,
        seeded: &crate::test_support::Seeded,
        connection: Uuid,
    ) -> Uuid {
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    issues::insert_source(
                        conn,
                        &issues::SourceRow {
                            id,
                            project_id: project,
                            name: "embeddings".to_owned(),
                            kind: issues::SourceKind::Score,
                            filter: None,
                            enabled: true,
                            paused_reason: None,
                            paused_at: None,
                            created_by: None,
                            created_at: now,
                            updated_at: now,
                            sample_rate: None,
                            daily_cap: None,
                            score_names: Vec::new(),
                            feedback_sources: Vec::new(),
                            chat_connection_id: None,
                            chat_model: None,
                            embed_connection_id: Some(connection),
                            embed_model: Some("text-embedding-test".to_owned()),
                            embed_dimension: None,
                            floor: None,
                            min_issue_size: None,
                            budget_usd: None,
                            budget_period: None,
                            paused_detail: None,
                            next_refit_at: None,
                        },
                    )
                    .await?;
                    issues::pause_sources_of_connection(conn, connection, Some("bad key"), now)
                        .await?;
                    Ok::<_, MetaError>(id)
                })
            })
            .await
            .unwrap()
    }

    async fn source_paused_reason(app: &TestApp, id: Uuid) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT paused_reason FROM issue_sources WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_successful_embedding_test_resumes_source_pauses() {
        let mock = MockLlm::start_with_embeddings(Vec::new(), vec![embedding(&[0.6, 0.8])]).await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        let id = mocked(&app, &seeded.cookie, &mock).await;
        let source = embedding_source_paused(&app, &seeded, id).await;
        assert_eq!(
            source_paused_reason(&app, source).await.as_deref(),
            Some("connection")
        );

        let response = test(
            &app,
            &seeded.cookie,
            id,
            &json!({"embedding_model": "text-embedding-test"}),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result = response.json::<LlmConnectionTestResult>();
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.model, "text-embedding-test");
        assert_eq!(source_paused_reason(&app, source).await, None);
    }

    #[tokio::test]
    async fn an_embedding_test_on_anthropic_is_refused_without_a_call() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create(
            &app,
            &seeded.cookie,
            &json!({"name": "Claude", "adapter": "anthropic", "api_key": "sk-ant-test-0123456789"}),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);

        let response = test(
            &app,
            &seeded.cookie,
            id,
            &json!({"embedding_model": "text-embedding-test"}),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result = response.json::<LlmConnectionTestResult>();
        assert!(!result.ok);
        let error = result.error.expect("a refused test carries its error");
        assert!(matches!(error.class, TestErrorClass::Permanent));
        assert_eq!(error.message, "This provider has no embeddings API.");
    }

    #[tokio::test]
    async fn a_key_change_resumes_and_sets_ok() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create(
            &app,
            &seeded.cookie,
            &json!({"name": "Keyed", "adapter": "openai", "api_key": "sk-test-key-0123456789"}),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    connections::set_connection_status(conn, id, "failing", Some("401"), 1).await
                })
            })
            .await
            .unwrap();
        let (by_connection, by_errors) = paused_rules(&app, &seeded, id).await;

        let patched = patch(
            &app,
            &seeded.cookie,
            id,
            &json!({"api_key": "sk-new-key-0123456789"}),
        )
        .await;
        assert_eq!(patched.status, 200, "{}", text(&patched));
        let status: String = sqlx::query_scalar("SELECT status FROM llm_connections WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(status, "ok");
        assert_eq!(paused_reason_of(&app, by_connection).await, None);
        assert_eq!(
            paused_reason_of(&app, by_errors).await.as_deref(),
            Some(PAUSE_ERRORS)
        );
    }

    #[tokio::test]
    async fn a_key_change_records_resumed_rules() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create(
            &app,
            &seeded.cookie,
            &json!({"name": "Resumed", "adapter": "openai", "api_key": "sk-test-key-0123456789"}),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        let (by_connection, _by_errors) = paused_rules(&app, &seeded, id).await;

        let patched = patch(
            &app,
            &seeded.cookie,
            id,
            &json!({"api_key": "sk-new-key-0123456789"}),
        )
        .await;
        assert_eq!(patched.status, 200, "{}", text(&patched));
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT details, target_kind, target_id FROM audit_events \
             WHERE action = 'online_rule.resumed'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (details, target_kind, target_id) = &rows[0];
        let details: Value = serde_json::from_str(details).unwrap();
        assert_eq!(details["reason"], json!("connection"));
        assert_eq!(target_kind, "online_rule");
        assert_eq!(target_id, &by_connection.to_string());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn api_key_rules() {
        let (app, cookie) = app().await;
        let too_long = format!("sk-{}", "x".repeat(4097 - 3));
        for bad in [too_long.as_str(), " sk-x", "sk-x ", "sk\u{7}x"] {
            let response = create(
                &app,
                &cookie,
                &json!({ "name": "A", "adapter": "openai", "api_key": bad }),
            )
            .await;
            assert_eq!(response.status, 400, "{bad}");
            let problem = response.problem();
            assert_eq!(problem.errors.len(), 1);
            assert_eq!(problem.errors[0].field, "api_key");
        }
        let ok = create(
            &app,
            &cookie,
            &json!({ "name": "A", "adapter": "openai", "api_key": "x".repeat(4096) }),
        )
        .await;
        assert_eq!(ok.status, 201, "{}", text(&ok));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_url_rules() {
        let (app, cookie) = app().await;
        let too_long = format!(
            "https://x.com/{}",
            "a".repeat(2049 - "https://x.com/".len())
        );
        for bad in [
            "https://x.com/v1#a",
            "http:///v1",
            too_long.as_str(),
            "https://x.com/v1?key=abc",
            // `Url::parse` strips tab, CR and LF; the raw value is refused.
            "https://x.com/a\nb",
        ] {
            let response = create(
                &app,
                &cookie,
                &json!({ "name": "A", "adapter": "openai", "api_key": "sk-test-0123456789abcdef", "base_url": bad }),
            )
            .await;
            assert_eq!(response.status, 400, "{bad}");
            let problem = response.problem();
            assert_eq!(problem.errors.len(), 1);
            assert_eq!(problem.errors[0].field, "base_url");
        }
        let ok = create(
            &app,
            &cookie,
            &json!({
                "name": "A",
                "adapter": "openai",
                "api_key": "sk-test-0123456789abcdef",
                "base_url": "https://x.com/v1?api-version=2024-10-21"
            }),
        )
        .await;
        assert_eq!(ok.status, 201, "{}", text(&ok));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrency_bounds() {
        let (app, cookie) = app().await;
        for good in [1, 64] {
            let response = create(
                &app,
                &cookie,
                &json!({ "name": format!("Bounds-{good}"), "adapter": "openai", "api_key": "sk-test-0123456789abcdef", "max_concurrency": good }),
            )
            .await;
            assert_eq!(response.status, 201, "{good}");
        }
        let bad = create(
            &app,
            &cookie,
            &json!({ "name": "B", "adapter": "openai", "api_key": "sk-test-0123456789abcdef", "max_concurrency": 65 }),
        )
        .await;
        assert_eq!(bad.status, 400);
        assert_eq!(bad.problem().errors[0].field, "max_concurrency");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_name_conflict_is_409() {
        let (app, cookie) = app().await;
        let first = create(&app, &cookie, &body("First")).await;
        assert_eq!(first.status, 201);
        let second = create(&app, &cookie, &body("Second")).await;
        assert_eq!(second.status, 201);
        let id = created_id(&second);
        let response = patch(&app, &cookie, id, &json!({ "name": "First" })).await;
        assert_eq!(response.status, 409);
        assert_eq!(
            response.problem().detail.as_deref(),
            Some("a connection with this name already exists")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn other_org_patch_and_delete_are_404() {
        let (app, cookie) = app().await;
        let other = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &cookie)
            .json(&json!({ "name": "Other" }))
            .send()
            .await;
        assert_eq!(other.status, 201, "{}", text(&other));
        let slug = other.json::<Value>()["slug"].as_str().unwrap().to_owned();
        let created = app
            .request(
                Method::POST,
                &format!("/api/v1/orgs/{slug}/llm-connections"),
            )
            .header(header::COOKIE.as_str(), &cookie)
            .json(&body("Elsewhere"))
            .send()
            .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);

        let patched = patch(&app, &cookie, id, &json!({ "name": "Renamed" })).await;
        assert_eq!(patched.status, 404);
        let deleted = delete(&app, &cookie, id).await;
        assert_eq!(deleted.status, 404);

        let still = app
            .request(
                Method::GET,
                &format!("/api/v1/orgs/{slug}/llm-connections/{id}"),
            )
            .header(header::COOKIE.as_str(), &cookie)
            .send()
            .await;
        assert_eq!(still.status, 200);
        assert_eq!(still.json::<LlmConnection>().name, "Elsewhere");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_pages_by_name() {
        let (app, cookie) = app().await;
        for name in ["c", "a", "b"] {
            let response = create(&app, &cookie, &body(name)).await;
            assert_eq!(response.status, 201);
        }
        let first = app
            .request(Method::GET, "/api/v1/orgs/acme/llm-connections?limit=1")
            .header(header::COOKIE.as_str(), &cookie)
            .send()
            .await;
        assert_eq!(first.status, 200);
        let first: Page<LlmConnection> = first.json();
        assert_eq!(first.data[0].name, "a");
        let cursor = first.next_cursor.expect("another page");

        let second = app
            .request(
                Method::GET,
                &format!("/api/v1/orgs/acme/llm-connections?limit=1&cursor={cursor}"),
            )
            .header(header::COOKIE.as_str(), &cookie)
            .send()
            .await;
        let second: Page<LlmConnection> = second.json();
        assert_eq!(second.data[0].name, "b");
        let cursor = second.next_cursor.expect("a last page");

        let third = app
            .request(
                Method::GET,
                &format!("/api/v1/orgs/acme/llm-connections?limit=1&cursor={cursor}"),
            )
            .header(header::COOKIE.as_str(), &cookie)
            .send()
            .await;
        let third: Page<LlmConnection> = third.json();
        assert_eq!(third.data[0].name, "c");
        assert!(third.next_cursor.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_creates_and_reads_back_without_the_key() {
        let (app, cookie) = app().await;
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/api/requests/llm-connection-create.json"
        );
        let request: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
        let created = create(&app, &cookie, &request).await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let response_text = text(&created);
        assert!(!response_text.contains("sk-EXAMPLE"), "{response_text}");
        assert!(!response_text.contains("\"api_key\""), "{response_text}");
        let connection: LlmConnection = created.json();
        assert_eq!(connection.api_key_hint, "WXYZ");
        assert_eq!(connection.name, "OpenAI");

        let got = get(&app, &cookie, connection.id).await;
        assert_eq!(got.status, 200);
        assert_eq!(got.json::<LlmConnection>(), connection);

        let list = app
            .request(Method::GET, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &cookie)
            .send()
            .await;
        assert_eq!(list.status, 200);
        let page: Page<LlmConnection> = list.json();
        assert_eq!(page.data, vec![connection]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_stored_key_is_sealed_with_its_row_id() {
        let (app, cookie) = app().await;
        let key = "sk-sealed-0123456789";
        let created = create(
            &app,
            &cookie,
            &json!({ "name": "Sealed", "adapter": "openai", "api_key": key }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        let stored = stored_secret(&app, id).await;
        assert!(
            !stored
                .windows(key.len())
                .any(|window| window == key.as_bytes()),
            "the plaintext key is in the stored bytes"
        );
        let opened = app
            .state
            .secrets
            .open(
                &stored,
                Aad {
                    table: "llm_connections",
                    column: "secret_enc",
                    row_id: &id,
                },
            )
            .unwrap();
        assert_eq!(opened, key.as_bytes());
        let moved = app.state.secrets.open(
            &stored,
            Aad {
                table: "llm_connections",
                column: "secret_enc",
                row_id: &Uuid::now_v7(),
            },
        );
        assert!(moved.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hint_boundaries() {
        let (app, cookie) = app().await;
        let short = create(
            &app,
            &cookie,
            &json!({ "name": "Short", "adapter": "openai", "api_key": "x".repeat(15) }),
        )
        .await;
        assert_eq!(short.json::<LlmConnection>().api_key_hint, "");
        let sixteen = create(
            &app,
            &cookie,
            &json!({ "name": "Sixteen", "adapter": "openai", "api_key": "abcdefghijklmnop" }),
        )
        .await;
        assert_eq!(sixteen.json::<LlmConnection>().api_key_hint, "mnop");
        let accents = create(
            &app,
            &cookie,
            &json!({ "name": "Accents", "adapter": "openai", "api_key": format!("{}éèàü", "k".repeat(12)) }),
        )
        .await;
        assert_eq!(accents.json::<LlmConnection>().api_key_hint, "éèàü");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validation_reports_every_field() {
        let (app, cookie) = app().await;
        let mut headers = serde_json::Map::new();
        for index in 0..21 {
            headers.insert(format!("x-{index:02}"), Value::String("v".to_owned()));
        }
        let request = json!({
            "name": "  ",
            "adapter": "openai",
            "api_key": "sk-test-0123456789abcdef",
            "base_url": "ftp://x",
            "default_model": "a/b",
            "max_concurrency": 0,
            "requests_per_minute": 20000,
            "extra_headers": Value::Object(headers),
        });
        let response = create(&app, &cookie, &request).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem = response.problem();
        let mut got: Vec<(String, String)> = problem
            .errors
            .iter()
            .map(|error| (error.field.clone(), error.message.clone()))
            .collect();
        got.sort();
        let mut expected: Vec<(String, String)> = [
            ("name", "name must be 1 to 100 characters"),
            ("base_url", "base_url must be an http or https URL without credentials, fragment or query other than api-version"),
            ("default_model", "default_model must be 1 to 200 characters without spaces or / \\ % ? #"),
            ("max_concurrency", "max_concurrency must be between 1 and 64"),
            ("requests_per_minute", "requests_per_minute must be between 1 and 10000"),
            ("extra_headers", "at most 20 extra headers"),
        ]
        .into_iter()
        .map(|(field, message)| (field.to_owned(), message.to_owned()))
        .collect();
        expected.sort();
        assert_eq!(got, expected);

        let list = app
            .request(Method::GET, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &cookie)
            .send()
            .await;
        let page: Page<LlmConnection> = list.json();
        assert!(page.data.is_empty(), "nothing was stored");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_url_credentials_are_refused() {
        let (app, cookie) = app().await;
        let response = create(
            &app,
            &cookie,
            &json!({
                "name": "A",
                "adapter": "openai",
                "api_key": "sk-test-0123456789abcdef",
                "base_url": "https://u:p@x.com"
            }),
        )
        .await;
        assert_eq!(response.status, 400);
        let problem = response.problem();
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, "base_url");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn header_rules() {
        let (app, cookie) = app().await;
        let owned = create(
            &app,
            &cookie,
            &json!({ "name": "A", "adapter": "openai", "api_key": "sk-test-0123456789abcdef", "extra_headers": { "Content-Type": "application/json" } }),
        )
        .await;
        assert_eq!(owned.status, 400);
        let problem = owned.problem();
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, "extra_headers.content-type");
        assert_eq!(problem.errors[0].message, "invalid header");

        let bad_name = create(
            &app,
            &cookie,
            &json!({ "name": "A", "adapter": "openai", "api_key": "sk-test-0123456789abcdef", "extra_headers": { "bad name": "v" } }),
        )
        .await;
        assert_eq!(bad_name.status, 400);
        let problem = bad_name.problem();
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, "extra_headers");
        assert_eq!(problem.errors[0].message, "invalid header name");

        let duplicate = create(
            &app,
            &cookie,
            &json!({ "name": "A", "adapter": "openai", "api_key": "sk-test-0123456789abcdef", "extra_headers": { "X-A": "1", "x-a": "2" } }),
        )
        .await;
        assert_eq!(duplicate.status, 400);
        let problem = duplicate.problem();
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, "extra_headers.x-a");

        let ok = create(
            &app,
            &cookie,
            &json!({ "name": "A", "adapter": "openai", "api_key": "sk-test-0123456789abcdef", "extra_headers": { "X-B": "2", "x-a": "1" } }),
        )
        .await;
        assert_eq!(ok.status, 201, "{}", text(&ok));
        let connection: LlmConnection = ok.json();
        assert_eq!(connection.extra_header_names, ["x-a", "x-b"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_name_is_409() {
        let (app, cookie) = app().await;
        let first = create(&app, &cookie, &body("Twin")).await;
        assert_eq!(first.status, 201);
        let second = create(&app, &cookie, &body("Twin")).await;
        assert_eq!(second.status, 409);
        assert_eq!(
            second.problem().detail.as_deref(),
            Some("a connection with this name already exists")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_keeps_absent_fields_and_clears_with_empty_strings() {
        let (app, cookie) = app().await;
        let first_key = "sk-first-0123456789";
        let created = create(
            &app,
            &cookie,
            &json!({
                "name": "Keep",
                "adapter": "openai",
                "api_key": first_key,
                "default_model": "gpt-5-mini"
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let connection: LlmConnection = created.json();
        let id = connection.id;
        assert_eq!(connection.api_key_hint, "6789");

        let cleared = patch(&app, &cookie, id, &json!({ "default_model": "" })).await;
        assert_eq!(cleared.status, 200, "{}", text(&cleared));
        let cleared: LlmConnection = cleared.json();
        assert_eq!(cleared.default_model, None);
        assert_eq!(cleared.api_key_hint, "6789");
        let stored = stored_secret(&app, id).await;
        let opened = app
            .state
            .secrets
            .open(
                &stored,
                Aad {
                    table: "llm_connections",
                    column: "secret_enc",
                    row_id: &id,
                },
            )
            .unwrap();
        assert_eq!(opened, first_key.as_bytes());

        let rekeyed = patch(
            &app,
            &cookie,
            id,
            &json!({ "api_key": "sk-NEWNEWNEWNEWNEW1234" }),
        )
        .await;
        assert_eq!(rekeyed.status, 200, "{}", text(&rekeyed));
        let rekeyed: LlmConnection = rekeyed.json();
        assert_eq!(rekeyed.api_key_hint, "1234");

        app.state.clock.advance(Duration::from_secs(1));
        let untouched = patch(&app, &cookie, id, &json!({})).await;
        assert_eq!(untouched.status, 200, "{}", text(&untouched));
        let untouched: LlmConnection = untouched.json();
        let mut expected = rekeyed.clone();
        expected.updated_at = untouched.updated_at;
        assert_eq!(untouched, expected);
        assert_eq!(untouched.updated_at, rekeyed.updated_at + 1_000_000_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_only_checks_present_fields() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("Stored")).await;
        assert_eq!(created.status, 201);
        let id = created_id(&created);
        let response = patch(&app, &cookie, id, &json!({ "max_concurrency": 0 })).await;
        assert_eq!(response.status, 400);
        let problem = response.problem();
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, "max_concurrency");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_rejects_each_invalid_field() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("Stored")).await;
        assert_eq!(created.status, 201);
        let id = created_id(&created);
        for (field, body) in [
            (
                "base_url",
                json!({ "base_url": "ftp://x", "api_key": "sk-test-0123456789abcdef" }),
            ),
            ("default_model", json!({ "default_model": "a/b" })),
            ("api_key", json!({ "api_key": " sk-x" })),
            (
                "extra_headers.content-type",
                json!({ "extra_headers": { "content-type": "v" } }),
            ),
        ] {
            let response = patch(&app, &cookie, id, &body).await;
            assert_eq!(response.status, 400, "{field}");
            let problem = response.problem();
            assert_eq!(problem.errors.len(), 1, "{field}");
            assert_eq!(problem.errors[0].field, field);
        }
        // Nothing was stored.
        let got: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(got.name, "Stored");
        assert_eq!(got.base_url, None);
        assert_eq!(got.default_model, None);
        assert!(got.extra_header_names.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_base_url_clears_with_empty_or_whitespace() {
        let (app, cookie) = app().await;
        let created = create(
            &app,
            &cookie,
            &json!({
                "name": "Cleared",
                "adapter": "openai",
                "api_key": "sk-test-0123456789abcdef",
                "base_url": "https://x.com/v1",
                "default_model": "gpt-test"
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);

        // A whitespace-only value clears the field, like "".
        let patched = patch(
            &app,
            &cookie,
            id,
            &json!({ "base_url": "   ", "api_key": "sk-test-0123456789abcdef" }),
        )
        .await;
        assert_eq!(patched.status, 200, "{}", text(&patched));
        let patched: LlmConnection = patched.json();
        assert_eq!(patched.base_url, None);
        let cleared = patch(&app, &cookie, id, &json!({ "default_model": "  " })).await;
        assert_eq!(cleared.status, 200, "{}", text(&cleared));
        assert_eq!(cleared.json::<LlmConnection>().default_model, None);

        // Whitespace around a value is trimmed, leading like trailing.
        let set = patch(
            &app,
            &cookie,
            id,
            &json!({ "base_url": "  https://x.com/v2 ", "api_key": "sk-test-0123456789abcdef" }),
        )
        .await;
        assert_eq!(set.status, 200, "{}", text(&set));
        assert_eq!(
            set.json::<LlmConnection>().base_url.as_deref(),
            Some("https://x.com/v2")
        );
    }

    /// Q1 (2026-11-23): a PATCH of `base_url` or `adapter` without `api_key` keeps the saved key.
    /// The key is sealed under the row, so the stored bytes do not change either.
    #[tokio::test(flavor = "multi_thread")]
    async fn patch_of_the_host_keeps_the_saved_key() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("Stored")).await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        let sealed = stored_secret(&app, id).await;
        let hint: LlmConnection = get(&app, &cookie, id).await.json();
        for change in [
            json!({ "base_url": "https://proxy.example.com/v1" }),
            json!({ "base_url": "" }),
            json!({ "adapter": "anthropic" }),
        ] {
            let response = patch(&app, &cookie, id, &change).await;
            assert_eq!(response.status, 200, "{change}: {}", text(&response));
            let patched: LlmConnection = response.json();
            assert_eq!(patched.api_key_hint, hint.api_key_hint, "{change}");
            assert_eq!(stored_secret(&app, id).await, sealed, "{change}");
        }
        let got: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(got.base_url, None);
        assert_eq!(got.adapter.as_str(), "anthropic");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_unknown_field_is_400() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("Stored")).await;
        let id = created_id(&created);
        let response = patch(&app, &cookie, id, &json!({ "api_keyy": "x" })).await;
        assert_eq!(response.status, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_extra_headers_replaces_and_clears() {
        let (app, cookie) = app().await;
        let created = create(
            &app,
            &cookie,
            &json!({
                "name": "Headers",
                "adapter": "openai",
                "api_key": "sk-test-0123456789abcdef",
                "extra_headers": { "x-a": "1", "x-b": "2" }
            }),
        )
        .await;
        assert_eq!(created.status, 201);
        let connection: LlmConnection = created.json();
        let id = connection.id;
        assert_eq!(connection.extra_header_names, ["x-a", "x-b"]);

        let replaced = patch(
            &app,
            &cookie,
            id,
            &json!({ "extra_headers": { "x-c": "3" } }),
        )
        .await;
        assert_eq!(replaced.status, 200, "{}", text(&replaced));
        let replaced: LlmConnection = replaced.json();
        assert_eq!(replaced.extra_header_names, ["x-c"]);
        let stored = stored_headers(&app, id).await.expect("a sealed map");
        let opened = app
            .state
            .secrets
            .open(
                &stored,
                Aad {
                    table: "llm_connections",
                    column: "extra_headers_enc",
                    row_id: &id,
                },
            )
            .unwrap();
        let map: BTreeMap<String, String> = serde_json::from_slice(&opened).unwrap();
        assert_eq!(map, BTreeMap::from([("x-c".to_owned(), "3".to_owned())]));

        let cleared = patch(&app, &cookie, id, &json!({ "extra_headers": {} })).await;
        assert_eq!(cleared.status, 200);
        let cleared: LlmConnection = cleared.json();
        assert!(cleared.extra_header_names.is_empty());
        assert_eq!(stored_headers(&app, id).await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_extra_headers_keeps_the_key_and_hint() {
        let (app, cookie) = app().await;
        let key = "sk-headers-0123456789";
        let created = create(
            &app,
            &cookie,
            &json!({
                "name": "Headers",
                "adapter": "openai",
                "api_key": key,
                "extra_headers": { "x-a": "1" }
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);

        let patched = patch(
            &app,
            &cookie,
            id,
            &json!({ "extra_headers": { "x-b": "2" } }),
        )
        .await;
        assert_eq!(patched.status, 200, "{}", text(&patched));
        let patched: LlmConnection = patched.json();
        assert_eq!(patched.api_key_hint, "6789");
        assert_eq!(patched.extra_header_names, ["x-b"]);

        let stored = stored_secret(&app, id).await;
        let opened = app
            .state
            .secrets
            .open(
                &stored,
                Aad {
                    table: "llm_connections",
                    column: "secret_enc",
                    row_id: &id,
                },
            )
            .unwrap();
        assert_eq!(opened, key.as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_can_list_but_not_write() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("Stored")).await;
        let id = created_id(&created);
        let org = crate::meta::orgs::org_by_slug(app.state.meta.reader(), "acme")
            .await
            .unwrap()
            .expect("the seeded org");
        let (member_id, member) = signed_in(&app, "member@example.com").await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;

        let listed = app
            .request(Method::GET, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &member)
            .send()
            .await;
        assert_eq!(listed.status, 200);
        let page: Page<LlmConnection> = listed.json();
        assert_eq!(page.data.len(), 1);

        let post = create(&app, &member, &body("Nope")).await;
        assert_eq!(post.status, 403);
        let patched = patch(&app, &member, id, &json!({ "name": "Nope" })).await;
        assert_eq!(patched.status, 403);
        let deleted = delete(&app, &member, id).await;
        assert_eq!(deleted.status, 403);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn viewer_can_list() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("Stored")).await;
        assert_eq!(created.status, 201);
        let id = created_id(&created);
        let org = crate::meta::orgs::org_by_slug(app.state.meta.reader(), "acme")
            .await
            .unwrap()
            .expect("the seeded org");
        let (viewer_id, viewer) = signed_in(&app, "viewer@example.com").await;
        add_member(&app, org.id, viewer_id, OrgRole::Viewer).await;
        let listed = app
            .request(Method::GET, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &viewer)
            .send()
            .await;
        assert_eq!(listed.status, 200);
        let post = create(&app, &viewer, &body("Nope")).await;
        assert_eq!(post.status, 403);
        let patched = patch(&app, &viewer, id, &json!({ "name": "Nope" })).await;
        assert_eq!(patched.status, 403);
        let deleted = delete(&app, &viewer, id).await;
        assert_eq!(deleted.status, 403);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn other_org_is_404() {
        let (app, cookie) = app().await;
        let other = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &cookie)
            .json(&json!({ "name": "Other" }))
            .send()
            .await;
        assert_eq!(other.status, 201, "{}", text(&other));
        let slug = other.json::<Value>()["slug"].as_str().unwrap().to_owned();
        let created = app
            .request(
                Method::POST,
                &format!("/api/v1/orgs/{slug}/llm-connections"),
            )
            .header(header::COOKIE.as_str(), &cookie)
            .json(&body("Elsewhere"))
            .send()
            .await;
        assert_eq!(created.status, 201);
        let id = created_id(&created);
        let response = get(&app, &cookie, id).await;
        assert_eq!(response.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_then_get_is_404() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("Gone")).await;
        let id = created_id(&created);
        let deleted = delete(&app, &cookie, id).await;
        assert_eq!(deleted.status, 204);
        let fetched = get(&app, &cookie, id).await;
        assert_eq!(fetched.status, 404);
        let again = delete(&app, &cookie, id).await;
        assert_eq!(again.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pat_can_manage() {
        let (app, cookie) = app().await;
        let minted = app
            .request(Method::POST, "/api/v1/me/tokens")
            .header(header::COOKIE.as_str(), &cookie)
            .json(&json!({ "name": "t", "scopes": ["read", "write"] }))
            .send()
            .await;
        assert_eq!(minted.status, 201, "{}", text(&minted));
        let secret = minted.json::<Value>()["secret"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::AUTHORIZATION.as_str(), &format!("Bearer {secret}"))
            .json(&body("From a PAT"))
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_only_pat_cannot_write() {
        let (app, cookie) = app().await;
        let minted = app
            .request(Method::POST, "/api/v1/me/tokens")
            .header(header::COOKIE.as_str(), &cookie)
            .json(&json!({ "name": "ro", "scopes": ["read"] }))
            .send()
            .await;
        assert_eq!(minted.status, 201, "{}", text(&minted));
        let secret = minted.json::<Value>()["secret"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::AUTHORIZATION.as_str(), &format!("Bearer {secret}"))
            .json(&body("Nope"))
            .send()
            .await;
        assert_eq!(response.status, 403);
    }

    // Current-thread, like every `capture_logs` test: the subscriber is the
    // thread default, and a multi-thread runtime could move the task away
    // from it between awaits.
    #[tokio::test]
    async fn no_key_material_in_logs() {
        let (app, cookie) = app().await;
        let first = "sk-logcheck-one-01234567";
        let second = "sk-logcheck-two-01234567";
        let (_, logs) = capture_logs(async {
            let created = create(
                &app,
                &cookie,
                &json!({ "name": "Logged", "adapter": "openai", "api_key": first }),
            )
            .await;
            assert_eq!(created.status, 201);
            let id = created_id(&created);
            let patched = patch(&app, &cookie, id, &json!({ "api_key": second })).await;
            assert_eq!(patched.status, 200);
        })
        .await;
        assert!(!logs.contains(first), "{logs}");
        assert!(!logs.contains(second), "{logs}");
        // Both audit records are in the table, and no key text is in them.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT action, details FROM audit_events \
             WHERE action IN ('llm_connection.created', 'llm_connection.updated') ORDER BY seq",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        let actions: Vec<&str> = rows.iter().map(|(action, _)| action.as_str()).collect();
        assert_eq!(
            actions,
            ["llm_connection.created", "llm_connection.updated"]
        );
        for (_, details) in &rows {
            assert!(
                !details.contains(first) && !details.contains(second),
                "{details}"
            );
        }
    }

    #[tokio::test]
    async fn connection_key_change_is_flagged() {
        let (app, cookie) = app().await;
        let created = create(
            &app,
            &cookie,
            &json!({ "name": "Flagged", "adapter": "openai", "api_key": "sk-flag-one-0123456789" }),
        )
        .await;
        assert_eq!(created.status, 201);
        let id = created_id(&created);
        let renamed = patch(&app, &cookie, id, &json!({ "name": "Flagged 2" })).await;
        assert_eq!(renamed.status, 200);
        let rekeyed = patch(
            &app,
            &cookie,
            id,
            &json!({ "api_key": "sk-flag-two-0123456789" }),
        )
        .await;
        assert_eq!(rekeyed.status, 200);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT details FROM audit_events \
             WHERE action = 'llm_connection.updated' AND target_id = ? ORDER BY seq",
        )
        .bind(id.to_string())
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        let details: Vec<Value> = rows
            .iter()
            .map(|(details,)| serde_json::from_str(details).unwrap())
            .collect();
        assert_eq!(details.len(), 2);
        assert_eq!(details[0]["fields"], json!(["name"]));
        assert_eq!(details[0]["key_changed"], json!(false));
        assert_eq!(details[1]["fields"], json!(["api_key"]));
        assert_eq!(details[1]["key_changed"], json!(true));
        for (text,) in &rows {
            assert!(!text.contains("sk-flag-"), "{text}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_succeeds_and_uses_the_decrypted_key() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let created = create(
            &app,
            &cookie,
            &json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test",
                "extra_headers": { "x-extra": "one" }
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(result.ok);
        assert_eq!(result.model, "gpt-test");
        assert!(result.error.is_none());

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        assert_eq!(
            header_value(headers, "authorization"),
            "Bearer sk-test-key-0123456789"
        );
        assert_eq!(header_value(headers, "x-extra"), "one");
        assert_eq!(body["model"], "gpt-test");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_uses_the_body_model() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({ "model": "other-model" })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(result.ok);
        assert_eq!(result.model, "other-model");
        assert_eq!(mock.requests()[0].1["model"], "other-model");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_without_any_model_is_400() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let created = create(
            &app,
            &cookie,
            &json!({
                "name": "No default",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url()
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 400);
        let problem = response.problem();
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, "model");
        assert_eq!(
            problem.errors[0].message,
            "model is required when the connection has no default model"
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_401_marks_the_connection_failing() {
        let mock = MockLlm::start(vec![
            Reply {
                status: 401,
                body: json!({
                    "error": {"message": "bad key SECRETPAYLOAD", "type": "invalid_request_error"}
                }),
            },
            completion(r#"{"value": true}"#),
        ])
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let failed = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(failed.status, 200, "{}", text(&failed));
        let result: LlmConnectionTestResult = failed.json();
        assert!(!result.ok);
        let error = result.error.expect("the failure");
        assert_eq!(error.class, TestErrorClass::Pause);
        assert!(
            !error.message.contains("SECRETPAYLOAD"),
            "{}",
            error.message
        );

        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.status, ConnectionStatus::Failing);
        assert!(connection.last_error.is_some());

        let passed = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(passed.status, 200, "{}", text(&passed));
        let result: LlmConnectionTestResult = passed.json();
        assert!(result.ok);

        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.status, ConnectionStatus::Ok);
        assert!(connection.last_error.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_404_on_an_explicit_model_keeps_the_status() {
        let mock = MockLlm::start(vec![Reply {
            status: 404,
            body: json!({ "model": "nope" }),
        }])
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({ "model": "nope" })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        assert_eq!(
            result.error.expect("the failure").class,
            TestErrorClass::Pause
        );

        // A 404 for a model typed in the test says nothing about the key.
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.status, ConnectionStatus::Ok);
        assert!(connection.last_error.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_500_is_retryable_and_keeps_the_status() {
        let mock = MockLlm::start(vec![Reply {
            status: 500,
            body: json!({ "error": { "message": "overloaded" } }),
        }])
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        assert_eq!(
            result.error.expect("the failure").class,
            TestErrorClass::Retryable
        );

        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.status, ConnectionStatus::Ok);
        assert!(connection.last_error.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_status_write_keeps_the_test_result() {
        let mock = MockLlm::start(vec![Reply {
            status: 401,
            body: json!({
                "error": {"message": "bad key", "type": "invalid_request_error"}
            }),
        }])
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        // Abort every UPDATE of llm_connections: the status write after the
        // provider call fails while reads still work. Closing the writer is
        // not an option here: `MetaDb::close` consumes the database the app
        // still holds.
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    sqlx::query(
                        "CREATE TRIGGER block_status_update BEFORE UPDATE ON llm_connections \
                         BEGIN SELECT RAISE(ABORT, 'blocked'); END",
                    )
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let failed = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(failed.status, 200, "{}", text(&failed));
        let result: LlmConnectionTestResult = failed.json();
        assert!(!result.ok);
        assert_eq!(
            result.error.expect("the failure").class,
            TestErrorClass::Pause
        );

        // The failing status never landed.
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.status, ConnectionStatus::Ok);
        assert!(connection.last_error.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_private_address_is_refused_without_the_setting() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        // The default config denies private-network egress.
        let (app, cookie) = app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        assert_eq!(
            result.error.expect("the failure").class,
            TestErrorClass::Permanent
        );
        assert!(mock.requests().is_empty(), "the mock saw no request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_uses_the_patched_key() {
        let mock = MockLlm::start(vec![
            completion(r#"{"value": true}"#),
            completion(r#"{"value": true}"#),
        ])
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let first = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(first.status, 200);
        assert!(first.json::<LlmConnectionTestResult>().ok);

        let patched = patch(
            &app,
            &cookie,
            id,
            &json!({ "api_key": "sk-second-key-0123456789" }),
        )
        .await;
        assert_eq!(patched.status, 200, "{}", text(&patched));

        let second = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(second.status, 200, "{}", text(&second));
        assert!(second.json::<LlmConnectionTestResult>().ok);
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            header_value(&requests[1].0, "authorization"),
            "Bearer sk-second-key-0123456789"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_after_delete_is_404() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        let deleted = delete(&app, &cookie, id).await;
        assert_eq!(deleted.status, 204);
        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_cannot_test() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        let org = crate::meta::orgs::org_by_slug(app.state.meta.reader(), "acme")
            .await
            .unwrap()
            .expect("the seeded org");
        let (member_id, member) = signed_in(&app, "member@example.com").await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;

        let response = test(&app, &member, id, &json!({})).await;
        assert_eq!(response.status, 403);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tests_are_rate_limited() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        for _ in 0..10 {
            let response = test(&app, &cookie, id, &json!({})).await;
            assert_eq!(response.status, 200, "{}", text(&response));
        }
        // The manual clock does not move: the 11th test in the minute is refused.
        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 429);
    }

    #[tokio::test]
    async fn test_connection_is_refused_in_restore_mode() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
            config.restore_mode = true;
        })
        .await;
        let seeded = app.seed().await;
        let id = mocked(&app, &seeded.cookie, &mock).await;
        let response = test(&app, &seeded.cookie, id, &json!({})).await;
        crate::http::restore::tests::assert_refused(&response, "connection test");
        assert!(mock.requests().is_empty());
    }

    /// The last test, as the API returns it: `(last_tested_at, last_test_ok)`.
    async fn last_test(app: &TestApp, cookie: &str, id: Uuid) -> (Option<Value>, Option<Value>) {
        let body: Value = get(app, cookie, id).await.json();
        (
            body.get("last_tested_at").cloned(),
            body.get("last_test_ok").cloned(),
        )
    }

    #[tokio::test]
    async fn a_new_connection_has_no_last_test() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create(&app, &seeded.cookie, &body("Fresh")).await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        assert_eq!(last_test(&app, &seeded.cookie, id).await, (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_saved_test_records_the_last_test() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert!(connection.last_tested_at.is_some());
        assert_eq!(connection.last_test_ok, Some(true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_saved_test_records_a_failed_last_test() {
        let mock = MockLlm::start(vec![Reply {
            status: 401,
            body: json!({
                "error": {"message": "bad key", "type": "invalid_request_error"}
            }),
        }])
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert!(connection.last_tested_at.is_some());
        assert_eq!(connection.last_test_ok, Some(false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_test_on_a_typed_model_records_nothing() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({"model": "other-model"})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);
        assert_eq!(last_test(&app, &cookie, id).await, (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_embedding_test_records_the_last_test() {
        let mock = MockLlm::start_with_embeddings(Vec::new(), vec![embedding(&[0.6, 0.8])]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(
            &app,
            &cookie,
            id,
            &json!({"embedding_model": "text-embedding-test"}),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.last_test_ok, Some(true));
    }

    #[tokio::test]
    async fn an_anthropic_embedding_refusal_records_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create(
            &app,
            &seeded.cookie,
            &json!({"name": "Claude", "adapter": "anthropic", "api_key": "sk-ant-test-0123456789"}),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);

        let response = test(
            &app,
            &seeded.cookie,
            id,
            &json!({"embedding_model": "text-embedding-test"}),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(!response.json::<LlmConnectionTestResult>().ok);
        assert_eq!(last_test(&app, &seeded.cookie, id).await, (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_patch_of_the_key_clears_the_last_test() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        assert_eq!(
            test(&app, &cookie, id, &json!({})).await.status,
            200,
            "the test"
        );

        let response = patch(
            &app,
            &cookie,
            id,
            &json!({"api_key": "sk-second-key-0123456789"}),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let connection: LlmConnection = response.json();
        assert_eq!(connection.last_tested_at, None);
        assert_eq!(connection.last_test_ok, None);
        assert_eq!(last_test(&app, &cookie, id).await, (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_patch_of_the_structured_output_clears_the_last_test() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        assert_eq!(
            test(&app, &cookie, id, &json!({})).await.status,
            200,
            "the test"
        );

        let response = patch(&app, &cookie, id, &json!({"structured_output": "tool"})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert_eq!(last_test(&app, &cookie, id).await, (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_patch_of_the_name_or_the_same_mode_keeps_the_last_test() {
        let mock = MockLlm::start(vec![completion(r#"{"value": true}"#)]).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        assert_eq!(
            test(&app, &cookie, id, &json!({})).await.status,
            200,
            "the test"
        );

        let renamed = patch(&app, &cookie, id, &json!({"name": "Renamed"})).await;
        assert_eq!(renamed.status, 200, "{}", text(&renamed));
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.last_test_ok, Some(true));

        let same_mode = patch(&app, &cookie, id, &json!({"structured_output": "native"})).await;
        assert_eq!(same_mode.status, 200, "{}", text(&same_mode));
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.last_test_ok, Some(true));
    }

    /// `POST …/llm-connections/test`: a test of a connection that is not saved.
    async fn test_unsaved(app: &TestApp, cookie: &str, body: &Value) -> TestResponse {
        app.request(Method::POST, "/api/v1/orgs/acme/llm-connections/test")
            .header(header::COOKIE.as_str(), cookie)
            .json(body)
            .send()
            .await
    }

    /// `n` replies of a plain judge answer.
    fn judge_replies(n: usize) -> Vec<Reply> {
        (0..n).map(|_| completion(r#"{"value": true}"#)).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_test_uses_the_saved_key_when_api_key_is_omitted() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({ "structured_output": "tool" })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            header_value(&requests[0].0, "authorization"),
            "Bearer sk-test-key-0123456789"
        );
        // A draft stores nothing.
        assert_eq!(last_test(&app, &cookie, id).await, (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_test_uses_a_new_key() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        let sealed = stored_secret(&app, id).await;

        let response = test(
            &app,
            &cookie,
            id,
            &json!({ "api_key": "sk-draft-key-0123456789" }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let requests = mock.requests();
        assert_eq!(
            header_value(&requests[0].0, "authorization"),
            "Bearer sk-draft-key-0123456789"
        );
        assert_eq!(stored_secret(&app, id).await, sealed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_draft_test_changes_no_status_pause_or_last_test() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        let id = mocked(&app, &seeded.cookie, &mock).await;
        let (by_connection, _) = paused_rules(&app, &seeded, id).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    connections::set_connection_status(conn, id, "failing", Some("bad key"), now)
                        .await
                })
            })
            .await
            .unwrap();

        let response = test(
            &app,
            &seeded.cookie,
            id,
            &json!({ "structured_output": "tool" }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);

        let got: LlmConnection = get(&app, &seeded.cookie, id).await.json();
        assert_eq!(got.status, ConnectionStatus::Failing);
        assert!(got.last_error.is_some());
        assert_eq!(got.last_tested_at, None);
        assert_eq!(
            paused_reason_of(&app, by_connection).await.as_deref(),
            Some(PAUSE_CONNECTION)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_draft_test_keeps_the_status() {
        let mock = MockLlm::start(vec![Reply {
            status: 401,
            body: json!({
                "error": {"message": "bad key", "type": "invalid_request_error"}
            }),
        }])
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(
            &app,
            &cookie,
            id,
            &json!({ "api_key": "sk-draft-key-0123456789" }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        assert_eq!(
            result.error.expect("the failure").class,
            TestErrorClass::Pause
        );

        let got: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(got.status, ConnectionStatus::Ok);
        assert_eq!(got.last_error, None);
        assert_eq!(got.last_tested_at, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_test_with_the_saved_values_is_not_a_draft() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({ "structured_output": "native" })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);
        assert_eq!(last_test(&app, &cookie, id).await.1, Some(json!(true)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_test_with_a_new_base_url_uses_the_saved_key() {
        let first = MockLlm::start(judge_replies(1)).await;
        let second = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &first).await;

        let response = test(&app, &cookie, id, &json!({ "base_url": second.base_url() })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);

        let requests = second.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            header_value(&requests[0].0, "authorization"),
            "Bearer sk-test-key-0123456789"
        );
        assert!(first.requests().is_empty());
        let got: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(got.base_url.as_deref(), Some(first.base_url().as_str()));
        assert_eq!(got.last_tested_at, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_patched_base_url_without_a_key_is_called_with_the_saved_key() {
        let first = MockLlm::start(judge_replies(1)).await;
        let second = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &first).await;

        let patched = patch(&app, &cookie, id, &json!({ "base_url": second.base_url() })).await;
        assert_eq!(patched.status, 200, "{}", text(&patched));

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);
        let requests = second.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            header_value(&requests[0].0, "authorization"),
            "Bearer sk-test-key-0123456789"
        );
        assert!(first.requests().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_test_rejects_an_invalid_base_url_and_headers() {
        let mock = MockLlm::start(judge_replies(10)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(
            &app,
            &cookie,
            id,
            &json!({
                "api_key": "sk-draft-key-0123456789",
                "base_url": "ftp://x",
                "extra_headers": { "content-type": "x" }
            }),
        )
        .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let fields: Vec<String> = response
            .problem()
            .errors
            .into_iter()
            .map(|error| error.field)
            .collect();
        assert!(fields.contains(&"base_url".to_owned()), "{fields:?}");
        assert!(
            fields.contains(&"extra_headers.content-type".to_owned()),
            "{fields:?}"
        );
        assert!(mock.requests().is_empty());

        // The refused body spent no test: ten valid tests still run.
        for _ in 0..10 {
            let response = test(&app, &cookie, id, &json!({})).await;
            assert_eq!(response.status, 200, "{}", text(&response));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_draft_for_an_unknown_id_is_400() {
        let (app, cookie) = app().await;
        let response = test(
            &app,
            &cookie,
            Uuid::now_v7(),
            &json!({ "base_url": "ftp://x" }),
        )
        .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert_eq!(response.problem().errors[0].field, "base_url");

        let response = test(&app, &cookie, Uuid::now_v7(), &json!({})).await;
        assert_eq!(response.status, 404, "{}", text(&response));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_draft_embedding_test_as_anthropic_is_refused() {
        let mock = MockLlm::start(Vec::new()).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(
            &app,
            &cookie,
            id,
            &json!({
                "adapter": "anthropic",
                "api_key": "sk-ant-draft-0123456789",
                "embedding_model": "e"
            }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        let error = result.error.expect("a refused test carries its error");
        assert_eq!(error.class, TestErrorClass::Permanent);
        assert_eq!(error.message, NO_EMBEDDINGS_MESSAGE);
        assert!(mock.requests().is_empty());
        assert!(mock.embedding_requests().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_test_calls_the_provider_with_the_body_values() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = egress_app().await;
        let response = test_unsaved(
            &app,
            &cookie,
            &json!({
                "adapter": "openai",
                "api_key": "sk-unsaved-0123456789",
                "base_url": mock.base_url(),
                "model": "gpt-test",
                "extra_headers": { "X-Extra": "one" }
            }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(result.ok, "{result:?}");
        assert_eq!(result.model, "gpt-test");

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        assert_eq!(
            header_value(headers, "authorization"),
            "Bearer sk-unsaved-0123456789"
        );
        assert_eq!(header_value(headers, "x-extra"), "one");
        assert_eq!(body["model"], "gpt-test");

        // Nothing was stored.
        let listed: Value = app
            .request(Method::GET, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &cookie)
            .send()
            .await
            .json();
        assert!(listed["data"].as_array().expect("a page").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_test_validates_like_create() {
        let (app, cookie) = egress_app().await;
        let response = test_unsaved(
            &app,
            &cookie,
            &json!({
                "adapter": "openai",
                "api_key": "",
                "base_url": "ftp://x",
                "extra_headers": { "host": "x" },
                "model": "m"
            }),
        )
        .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let fields: Vec<String> = response
            .problem()
            .errors
            .into_iter()
            .map(|error| error.field)
            .collect();
        for field in ["api_key", "base_url", "extra_headers.host"] {
            assert!(fields.contains(&field.to_owned()), "{fields:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_test_needs_a_model() {
        let (app, cookie) = egress_app().await;
        let response = test_unsaved(
            &app,
            &cookie,
            &json!({ "adapter": "openai", "api_key": "sk-unsaved-0123456789" }),
        )
        .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem = response.problem();
        assert_eq!(problem.errors[0].field, "model");
        assert_eq!(problem.errors[0].message, UNSAVED_MODEL_MESSAGE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_embedding_test_on_anthropic_is_refused() {
        let mock = MockLlm::start(Vec::new()).await;
        let (app, cookie) = egress_app().await;
        let response = test_unsaved(
            &app,
            &cookie,
            &json!({
                "adapter": "anthropic",
                "api_key": "sk-ant-unsaved-0123456789",
                "base_url": mock.base_url(),
                "embedding_model": "e"
            }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        assert_eq!(
            result
                .error
                .expect("a refused test carries its error")
                .message,
            NO_EMBEDDINGS_MESSAGE
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_test_to_a_private_address_is_refused_without_the_setting() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = app().await;
        let response = test_unsaved(
            &app,
            &cookie,
            &json!({
                "adapter": "openai",
                "api_key": "sk-unsaved-0123456789",
                "base_url": mock.base_url(),
                "model": "gpt-test"
            }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        assert_eq!(
            result.error.expect("the refusal").class,
            TestErrorClass::Permanent
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_and_saved_tests_share_the_rate_limit() {
        let mock = MockLlm::start(judge_replies(10)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;
        let unsaved = json!({
            "adapter": "openai",
            "api_key": "sk-unsaved-0123456789",
            "base_url": mock.base_url(),
            "model": "gpt-test",
            "extra_headers": { "X-Extra": "one" }
        });
        for _ in 0..5 {
            let response = test(&app, &cookie, id, &json!({})).await;
            assert_eq!(response.status, 200, "{}", text(&response));
            let response = test_unsaved(&app, &cookie, &unsaved).await;
            assert_eq!(response.status, 200, "{}", text(&response));
        }
        // The manual clock does not move: the 11th test in the minute is refused.
        let response = test_unsaved(&app, &cookie, &unsaved).await;
        assert_eq!(response.status, 429, "{}", text(&response));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_cannot_test_an_unsaved_connection() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let (app, _) = egress_app().await;
        let org = crate::meta::orgs::org_by_slug(app.state.meta.reader(), "acme")
            .await
            .unwrap()
            .expect("the seeded org");
        let (member_id, member) = signed_in(&app, "member@example.com").await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;

        let response = test_unsaved(
            &app,
            &member,
            &json!({
                "adapter": "openai",
                "api_key": "sk-unsaved-0123456789",
                "base_url": mock.base_url(),
                "model": "gpt-test"
            }),
        )
        .await;
        assert_eq!(response.status, 403, "{}", text(&response));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn unsaved_test_is_refused_in_restore_mode() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
            config.restore_mode = true;
        })
        .await;
        let seeded = app.seed().await;
        let response = test_unsaved(
            &app,
            &seeded.cookie,
            &json!({
                "adapter": "openai",
                "api_key": "sk-unsaved-0123456789",
                "base_url": mock.base_url(),
                "model": "gpt-test"
            }),
        )
        .await;
        crate::http::restore::tests::assert_refused(&response, "connection test");
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn no_key_material_in_logs_of_draft_tests() {
        // Private addresses, without egress: the calls are refused before any connection, so no
        // host name reaches the transport's own trace logs.
        let (app, cookie) = app().await;
        let created = create(
            &app,
            &cookie,
            &json!({
                "name": "Logged",
                "adapter": "openai",
                "api_key": "sk-test-0123456789abcdef",
                "base_url": "http://10.0.0.1:9/v1",
                "default_model": "gpt-test"
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        let (_, logs) = capture_logs(async {
            let response = test(
                &app,
                &cookie,
                id,
                &json!({
                    "api_key": "sk-logdraft-one-01234567",
                    "extra_headers": { "x-a": "hdr-secret-1" },
                    "model": "m"
                }),
            )
            .await;
            assert_eq!(response.status, 200, "{}", text(&response));
            let response = test_unsaved(
                &app,
                &cookie,
                &json!({
                    "adapter": "openai",
                    "api_key": "sk-logdraft-two-01234567",
                    "base_url": "http://10.0.0.2:9/v1",
                    "extra_headers": { "x-a": "hdr-secret-2" },
                    "model": "m"
                }),
            )
            .await;
            assert_eq!(response.status, 200, "{}", text(&response));
        })
        .await;
        for secret in [
            "sk-logdraft-one-01234567",
            "sk-logdraft-two-01234567",
            "hdr-secret-1",
            "hdr-secret-2",
            "10.0.0.2",
        ] {
            assert!(!logs.contains(secret), "{logs}");
        }
        assert!(logs.contains("llm connection tested"), "{logs}");
        assert!(logs.contains("llm connection draft tested"), "{logs}");
    }

    /// Replaces the stored key of `id` with bytes that do not open, as a key sealed under another
    /// secret would be. The rest of the row is unchanged.
    async fn corrupt_stored_key(app: &TestApp, id: Uuid) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE llm_connections SET secret_enc = X'00' WHERE id = ?")
                        .bind(id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok::<_, MetaError>(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_key_still_refuses_an_anthropic_embedding_test() {
        let (app, cookie) = app().await;
        let created = create(
            &app,
            &cookie,
            &json!({"name": "Claude", "adapter": "anthropic", "api_key": "sk-ant-test-0123456789"}),
        )
        .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        corrupt_stored_key(&app, id).await;

        let response = test(&app, &cookie, id, &json!({"embedding_model": "x"})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(!result.ok);
        let error = result.error.expect("a refused test carries its error");
        assert_eq!(error.class, TestErrorClass::Permanent);
        assert_eq!(error.message, NO_EMBEDDINGS_MESSAGE);
        assert_eq!(last_test(&app, &cookie, id).await, (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_key_still_answers_400_without_a_model() {
        let (app, cookie) = app().await;
        let created = create(&app, &cookie, &body("No default")).await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let id = created_id(&created);
        corrupt_stored_key(&app, id).await;

        let response = test(&app, &cookie, id, &json!({})).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert_eq!(response.problem().errors[0].field, "model");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_explicit_default_model_records_the_last_test() {
        let mock = MockLlm::start(judge_replies(1)).await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(&app, &cookie, id, &json!({ "model": "gpt-test" })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let result: LlmConnectionTestResult = response.json();
        assert!(result.ok);
        assert_eq!(result.model, "gpt-test");
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert!(connection.last_tested_at.is_some());
        assert_eq!(connection.last_test_ok, Some(true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_result_after_a_concurrent_patch_is_not_stored() {
        // The provider answers 401 after 300 ms; a PATCH saved meanwhile changes the row first.
        let mock = MockLlm::start_with_delay(
            vec![Reply {
                status: 401,
                body: json!({"error": {"message": "bad key", "type": "invalid_request_error"}}),
            }],
            Duration::from_millis(300),
        )
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let empty = json!({});
        let renamed = json!({ "name": "Renamed" });
        let (response, patched) = tokio::join!(test(&app, &cookie, id, &empty), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            // The manual clock must move, or the PATCH keeps the `updated_at` the test started from.
            app.state.clock.advance(Duration::from_secs(1));
            patch(&app, &cookie, id, &renamed).await
        });
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(!response.json::<LlmConnectionTestResult>().ok);
        assert_eq!(patched.status, 200, "{}", text(&patched));

        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert_eq!(connection.name, "Renamed");
        assert_eq!(connection.status, ConnectionStatus::Ok);
        assert_eq!(connection.last_error, None);
        assert_eq!(connection.last_tested_at, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_draft_embedding_test_stores_nothing() {
        let saved = MockLlm::start(Vec::new()).await;
        let draft = MockLlm::start_with_embeddings(Vec::new(), vec![embedding(&[0.6, 0.8])]).await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        let id = mocked(&app, &seeded.cookie, &saved).await;
        let (by_connection, _) = paused_rules(&app, &seeded, id).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    connections::set_connection_status(conn, id, "failing", Some("bad key"), now)
                        .await
                })
            })
            .await
            .unwrap();

        let response = test(
            &app,
            &seeded.cookie,
            id,
            &json!({
                "embedding_model": "text-embedding-test",
                "base_url": draft.base_url()
            }),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(response.json::<LlmConnectionTestResult>().ok);
        assert_eq!(draft.embedding_requests().len(), 1);
        assert!(saved.requests().is_empty());

        let got: LlmConnection = get(&app, &seeded.cookie, id).await.json();
        assert_eq!(got.base_url.as_deref(), Some(saved.base_url().as_str()));
        assert_eq!(got.status, ConnectionStatus::Failing);
        assert!(got.last_error.is_some());
        assert_eq!(got.last_tested_at, None);
        assert_eq!(
            paused_reason_of(&app, by_connection).await.as_deref(),
            Some(PAUSE_CONNECTION)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_saved_embedding_test_records_a_failed_last_test() {
        let mock = MockLlm::start_with_embeddings(
            Vec::new(),
            vec![Reply {
                status: 401,
                body: json!({"error": {"message": "bad key", "type": "invalid_request_error"}}),
            }],
        )
        .await;
        let (app, cookie) = egress_app().await;
        let id = mocked(&app, &cookie, &mock).await;

        let response = test(
            &app,
            &cookie,
            id,
            &json!({"embedding_model": "text-embedding-test"}),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert!(!response.json::<LlmConnectionTestResult>().ok);
        let connection: LlmConnection = get(&app, &cookie, id).await.json();
        assert!(connection.last_tested_at.is_some());
        assert_eq!(connection.last_test_ok, Some(false));
        assert_eq!(connection.status, ConnectionStatus::Ok);
    }
}
