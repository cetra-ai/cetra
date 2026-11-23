//! Model prices (spec 09 § Pricing & GDPR, D-T-6, D-T-7): project routes and org routes.

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use cetra_otel::{PriceRow, PriceScope};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::common::Page;
use crate::api::model_prices::{
    ModelPrice, ModelPriceInput, ModelPriceListParams, ModelPriceResolution,
    ModelPriceResolveParams,
};
use crate::audit;
use crate::auth::principal::{self, Principal};
use crate::auth::roles::{OrgPerm, ProjectPerm};
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::model_prices as store;
use crate::prices::{self, PriceChange, WriteOutcome, builtin_json, record_json};

use super::orgs::page_named;

/// Model price routes of a project. Reads need project view; writes need
/// [`ProjectPerm::ManageSettings`] (project admins; API keys cannot, D-T-6).
pub(crate) fn project_routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/models",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_models),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/models/resolve",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(resolve_model_price),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/models",
            access: Access::Project {
                perm: ProjectPerm::ManageSettings,
                session_only: false,
            },
            documented: true,
            handler: post(create_model_price),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/projects/{project}/models/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageSettings,
                session_only: false,
            },
            documented: true,
            handler: patch(update_model_price),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/v1/projects/{project}/models/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageSettings,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_model_price),
        },
    ]
}

/// Model price routes of an org (D-T-6, D-T-7). Reads need org view; writes need
/// [`OrgPerm::ManagePrices`] (org admins). Not session-only: a PAT of an admin may script them.
pub(crate) fn org_routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/orgs/{org}/models",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_org_model_prices),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/orgs/{org}/models",
            access: Access::Org {
                perm: OrgPerm::ManagePrices,
                session_only: false,
            },
            documented: true,
            handler: post(create_org_model_price),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/orgs/{org}/models/{id}",
            access: Access::Org {
                perm: OrgPerm::ManagePrices,
                session_only: false,
            },
            documented: true,
            handler: patch(update_org_model_price),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/v1/orgs/{org}/models/{id}",
            access: Access::Org {
                perm: OrgPerm::ManagePrices,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_org_model_price),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct PricePath {
    id: Uuid,
}

/// Sort rank of a row in the effective list (D-T-7): project, then org, then built-in.
const PROJECT_RANK: u8 = 0;
const ORG_RANK: u8 = 1;
const BUILTIN_RANK: u8 = 2;

async fn list_models(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<ModelPriceListParams>,
) -> Result<error::Json<Page<ModelPrice>>, ApiError> {
    let org = access.project.org_id;
    let project = access.project.id;
    let query = match params.q.as_deref().map(|q| q.trim().to_lowercase()) {
        None => None,
        Some(q) if q.is_empty() => None,
        Some(q) if q.chars().count() > 200 => {
            return Err(field_error("q", "at most 200 characters"));
        }
        Some(q) => Some(q),
    };
    let builtin = state.ingest.prices().builtin();

    let mut rows: Vec<(String, String, ModelPrice)> = Vec::new();
    for record in store::list_org_all(state.meta.reader(), org).await? {
        let rank = match record.project_id {
            None => ORG_RANK,
            Some(owner) if owner == project => PROJECT_RANK,
            Some(_) => continue,
        };
        rows.push((
            format!("{rank}:{}", record.row.model_name.to_lowercase()),
            record.id.to_string(),
            record_json(&record),
        ));
    }
    for (index, row) in builtin.rows().iter().enumerate() {
        rows.push((
            format!("{BUILTIN_RANK}:{}", row.model_name.to_lowercase()),
            format!("builtin:{index:06}"),
            builtin_json(row),
        ));
    }

    let mut rows: Vec<(String, String, ModelPrice)> = rows
        .into_iter()
        .filter(|(_, _, price)| params.scope.is_none_or(|scope| price.scope == scope))
        .filter(|(_, _, price)| {
            query
                .as_deref()
                .is_none_or(|q| price.model_name.to_lowercase().contains(q))
        })
        .collect();
    rows.sort_by(|(name_a, id_a, _), (name_b, id_b, _)| (name_a, id_a).cmp(&(name_b, id_b)));
    Ok(error::Json(page_named(
        rows,
        params.cursor.as_deref(),
        params.limit.get(),
    )?))
}

async fn resolve_model_price(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<ModelPriceResolveParams>,
) -> Result<error::Json<ModelPriceResolution>, ApiError> {
    let org = access.project.org_id;
    let project = access.project.id;
    let model = params.model.trim();
    if model.is_empty() {
        return Err(field_error("model", "is required"));
    }
    if model.len() > 500 {
        return Err(field_error("model", "at most 500 bytes"));
    }
    let provider = params
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty());
    let now = state.clock.now_ns();
    let table = state.ingest.prices().for_project(org, project);
    let Some(row) = table.price_for(model, provider, now) else {
        return Ok(error::Json(ModelPriceResolution { price: None }));
    };
    let price = if row.scope == PriceScope::Builtin {
        builtin_json(row)
    } else {
        stored_json(&state, org, project, row).await?
    };
    Ok(error::Json(ModelPriceResolution { price: Some(price) }))
}

/// The stored row that is `row` (a custom row of the table), with its id and times. A reload
/// that raced the lookup answers without them.
async fn stored_json(
    state: &AppState,
    org: cetra_core::ids::OrgId,
    project: cetra_core::ids::ProjectId,
    row: &PriceRow,
) -> Result<ModelPrice, ApiError> {
    let records = store::list_org_all(state.meta.reader(), org).await?;
    let found = records.iter().find(|record| {
        record.row == *row
            && match row.scope {
                PriceScope::Project => record.project_id == Some(project),
                _ => record.project_id.is_none(),
            }
    });
    Ok(match found {
        Some(record) => record_json(record),
        None => builtin_json(row),
    })
}

async fn create_model_price(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<ModelPriceInput>,
) -> Result<(StatusCode, error::Json<ModelPrice>), ApiError> {
    principal::session_user(&principal)?;
    let org = access.project.org_id;
    let project = access.project.id;
    let row = validated(&body, PriceScope::Project)?;
    let id = Uuid::now_v7();
    let outcome = prices::write(
        &state,
        org,
        Some(project),
        PriceChange::Insert { id, row },
        &ctx,
    )
    .await?;
    match outcome {
        WriteOutcome::Done(Some(record)) => {
            Ok((StatusCode::CREATED, error::Json(record_json(&record))))
        }
        other => Err(outcome_error(other)),
    }
}

async fn update_model_price(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(PricePath { id }): error::Path<PricePath>,
    error::Json(body): error::Json<ModelPriceInput>,
) -> Result<error::Json<ModelPrice>, ApiError> {
    principal::session_user(&principal)?;
    let org = access.project.org_id;
    let project = access.project.id;
    let row = validated(&body, PriceScope::Project)?;
    let outcome = prices::write(
        &state,
        org,
        Some(project),
        PriceChange::Update { id, row },
        &ctx,
    )
    .await?;
    match outcome {
        WriteOutcome::Done(Some(record)) => Ok(error::Json(record_json(&record))),
        other => Err(outcome_error(other)),
    }
}

async fn delete_model_price(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(PricePath { id }): error::Path<PricePath>,
) -> Result<StatusCode, ApiError> {
    principal::session_user(&principal)?;
    let org = access.project.org_id;
    let project = access.project.id;
    let outcome = prices::write(
        &state,
        org,
        Some(project),
        PriceChange::Delete { id },
        &ctx,
    )
    .await?;
    match outcome {
        WriteOutcome::Done(None) => Ok(StatusCode::NO_CONTENT),
        other => Err(outcome_error(other)),
    }
}

async fn list_org_model_prices(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<Page<ModelPrice>>, ApiError> {
    let rows = store::list_org_rows(state.meta.reader(), access.org.id).await?;
    Ok(error::Json(Page {
        data: rows.iter().map(record_json).collect(),
        next_cursor: None,
    }))
}

async fn create_org_model_price(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<ModelPriceInput>,
) -> Result<(StatusCode, error::Json<ModelPrice>), ApiError> {
    principal::session_user(&principal)?;
    let org = access.org.id;
    let row = validated(&body, PriceScope::Org)?;
    let id = Uuid::now_v7();
    let outcome =
        prices::write(&state, org, None, PriceChange::Insert { id, row }, &ctx).await?;
    match outcome {
        WriteOutcome::Done(Some(record)) => {
            Ok((StatusCode::CREATED, error::Json(record_json(&record))))
        }
        other => Err(outcome_error(other)),
    }
}

async fn update_org_model_price(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(PricePath { id }): error::Path<PricePath>,
    error::Json(body): error::Json<ModelPriceInput>,
) -> Result<error::Json<ModelPrice>, ApiError> {
    principal::session_user(&principal)?;
    let org = access.org.id;
    let row = validated(&body, PriceScope::Org)?;
    let outcome =
        prices::write(&state, org, None, PriceChange::Update { id, row }, &ctx).await?;
    match outcome {
        WriteOutcome::Done(Some(record)) => Ok(error::Json(record_json(&record))),
        other => Err(outcome_error(other)),
    }
}

async fn delete_org_model_price(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(PricePath { id }): error::Path<PricePath>,
) -> Result<StatusCode, ApiError> {
    principal::session_user(&principal)?;
    let org = access.org.id;
    let outcome = prices::write(&state, org, None, PriceChange::Delete { id }, &ctx).await?;
    match outcome {
        WriteOutcome::Done(None) => Ok(StatusCode::NO_CONTENT),
        other => Err(outcome_error(other)),
    }
}

/// The row of a body in `scope`, or the field errors of D-T-4 as `400`.
fn validated(body: &ModelPriceInput, scope: PriceScope) -> Result<PriceRow, ApiError> {
    prices::validate_input(body, scope).map_err(|errors| ApiError::BadRequest {
        detail: None,
        errors,
    })
}

/// The error of a write that did not store a row (D-T-8 and D-T-4).
fn outcome_error(outcome: WriteOutcome) -> ApiError {
    match outcome {
        WriteOutcome::Invalid(errors) => ApiError::BadRequest {
            detail: None,
            errors,
        },
        WriteOutcome::BreaksRules(errors) => ApiError::ConflictWith {
            detail: "the change would leave budgeted online rules without a price",
            errors,
        },
        WriteOutcome::NotFound | WriteOutcome::Done(_) => ApiError::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use cetra_core::ids::{OrgId, ProjectId};
    use cetra_otel::exact_pattern;
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::ingest::body::{BodyEncoding, BodyFormat};
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::members::add_org_member;
    use crate::meta::model_prices;
    use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::capture_logs;
    use crate::test_support::session_cookie_via;
    use crate::test_support::{Seeded, TestApp, TestResponse};
    use cetra_core::span::Span;

    fn path(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/models")
    }

    fn body(name: &str, input: f64, output: f64) -> Value {
        json!({"model_name": name, "price_input": input, "price_output": output})
    }

    async fn post(
        app: &TestApp,
        seeded: &Seeded,
        project: ProjectId,
        body: &Value,
    ) -> TestResponse {
        app.request(Method::POST, &path(project))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send()
            .await
    }

    async fn write_as(
        app: &TestApp,
        seeded: &Seeded,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> TestResponse {
        let request = app
            .request(method, url)
            .header(header::COOKIE.as_str(), &seeded.cookie);
        match body {
            Some(body) => request.json(body).send().await,
            None => request.send().await,
        }
    }

    async fn list(app: &TestApp, seeded: &Seeded, query: &str) -> TestResponse {
        app.request(Method::GET, &format!("{}{query}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
    }

    async fn resolve(app: &TestApp, seeded: &Seeded, query: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("{}/resolve{query}", path(seeded.project.id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
    }

    fn created_id(response: &TestResponse) -> String {
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json::<Value>()["id"]
            .as_str()
            .expect("id")
            .to_owned()
    }

    #[tokio::test]
    async fn create_list_update_delete_round_trip() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;

        let created = post(&app, &seeded, project, &body("llama3.1:8b", 0.0, 0.0)).await;
        assert_eq!(
            created.status,
            201,
            "{}",
            String::from_utf8_lossy(&created.body)
        );
        let created: Value = created.json();
        assert_eq!(created["scope"], "project");
        assert_eq!(created["match_pattern"], exact_pattern("llama3.1:8b"));
        assert_eq!(created["tiers"], json!([]));
        let id = created["id"].as_str().expect("id").to_owned();

        let page: Value = list(&app, &seeded, "?scope=project").await.json();
        assert_eq!(page["data"].as_array().map(Vec::len), Some(1));

        let url = format!("{}/{id}", path(project));
        let replaced = json!({
            "model_name": "llama3.1:8b",
            "price_input": 0.1,
            "price_output": 0.2,
            "provider": "openai"
        });
        let patched = write_as(&app, &seeded, Method::PATCH, &url, Some(&replaced)).await;
        assert_eq!(
            patched.status,
            200,
            "{}",
            String::from_utf8_lossy(&patched.body)
        );
        assert_eq!(patched.json::<Value>()["provider"], "openai");

        let without_provider = json!({
            "model_name": "llama3.1:8b",
            "price_input": 0.1,
            "price_output": 0.2
        });
        let patched = write_as(&app, &seeded, Method::PATCH, &url, Some(&without_provider)).await;
        assert_eq!(patched.status, 200);
        assert!(patched.json::<Value>().get("provider").is_none());

        let deleted = write_as(&app, &seeded, Method::DELETE, &url, None).await;
        assert_eq!(deleted.status, 204);
        let page: Value = list(&app, &seeded, "?scope=project").await.json();
        assert_eq!(page["data"], json!([]));
    }

    #[tokio::test]
    async fn the_list_orders_project_org_builtin_and_pages() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let now = app.state.clock.now_ns();
        let org_row = PriceRow {
            scope: PriceScope::Org,
            model_name: "Llama 3.1 8B (local)".to_owned(),
            match_pattern: exact_pattern("llama3.1:8b"),
            provider: None,
            valid_from: 0,
            price_input: Some(0.0),
            price_output: Some(0.0),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    model_prices::insert_tx(conn, Uuid::now_v7(), org, None, &org_row, now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("insert org row");
        post(
            &app,
            &seeded,
            seeded.project.id,
            &body("gpt-4o (negotiated)", 2.0, 8.0),
        )
        .await;

        let first: Value = list(&app, &seeded, "?limit=2").await.json();
        let data = first["data"].as_array().expect("data");
        assert_eq!(data[0]["scope"], "project");
        assert_eq!(data[1]["scope"], "org");
        let cursor = first["next_cursor"]
            .as_str()
            .expect("a next page")
            .to_owned();

        let second: Value = list(&app, &seeded, &format!("?limit=2&cursor={cursor}"))
            .await
            .json();
        assert_eq!(second["data"][0]["scope"], "builtin");

        let builtin: Value = list(&app, &seeded, "?scope=builtin&q=gpt-4o&limit=1000")
            .await
            .json();
        let builtin = builtin["data"].as_array().expect("data");
        assert!(!builtin.is_empty());
        assert!(builtin.iter().all(|row| {
            row["scope"] == "builtin"
                && row["model_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("gpt-4o")
        }));

        let too_long = "a".repeat(201);
        let response = list(&app, &seeded, &format!("?q={too_long}")).await;
        assert_eq!(response.status, 400);
    }

    #[tokio::test]
    async fn other_projects_rows_are_not_listed_or_used() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let created = post(&app, &seeded, other.id, &body("gpt-4o", 9.0, 9.0)).await;
        assert_eq!(created.status, 201);

        let page: Value = list(&app, &seeded, "?scope=project").await.json();
        assert_eq!(page["data"], json!([]));
        let found: Value = resolve(&app, &seeded, "?model=gpt-4o&provider=openai")
            .await
            .json();
        assert_eq!(found["price"]["scope"], "builtin");
    }

    #[tokio::test]
    async fn resolve_names_the_winning_row() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;

        let builtin: Value = resolve(&app, &seeded, "?model=gpt-4o&provider=openai")
            .await
            .json();
        assert_eq!(builtin["price"]["scope"], "builtin");

        let id =
            created_id(&post(&app, &seeded, seeded.project.id, &body("gpt-4o", 1.0, 1.0)).await);
        let project: Value = resolve(&app, &seeded, "?model=gpt-4o").await.json();
        assert_eq!(project["price"]["id"], id);
        assert_eq!(project["price"]["scope"], "project");

        let none: Value = resolve(&app, &seeded, "?model=no-such-model-0")
            .await
            .json();
        assert!(none.get("price").is_none());

        let blank = resolve(&app, &seeded, "?model=").await;
        assert_eq!(blank.status, 400);
    }

    #[tokio::test]
    async fn validation_errors_are_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let invalid = json!({"model_name": "", "match_pattern": "x)|(", "price_cache_read": 1.0});
        let response = post(&app, &seeded, seeded.project.id, &invalid).await;
        assert_eq!(response.status, 400);
        let fields: Vec<String> = response
            .problem()
            .errors
            .iter()
            .map(|error| error.field.clone())
            .collect();
        assert_eq!(fields, ["model_name", "match_pattern", "price_input"]);
    }

    #[tokio::test]
    async fn a_duplicate_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        created_id(&post(&app, &seeded, seeded.project.id, &body("gpt-4o", 1.0, 1.0)).await);
        let again = post(&app, &seeded, seeded.project.id, &body("gpt-4o", 2.0, 2.0)).await;
        assert_eq!(again.status, 409);
    }

    #[tokio::test]
    async fn deleting_the_price_of_a_budgeted_rule_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (org, project, now) = (seeded.org_id, seeded.project.id, app.state.clock.now_ns());
        let judge = Uuid::now_v7();
        let connection = Uuid::now_v7();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_connection(
                        conn,
                        connection,
                        org,
                        ConnectionFields {
                            name: format!("judge {connection}"),
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
                        },
                        None,
                        now,
                    )
                    .await?;
                    insert_evaluator(
                        conn,
                        judge,
                        project,
                        "llm_judge",
                        EvaluatorFields {
                            name: format!("judge {judge}"),
                            config: json!({
                                "connection_id": connection,
                                "model": "llama3.1:8b",
                                "prompt": "{{ input }} {{ output }}",
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
                    Ok(())
                })
            })
            .await
            .expect("connection and judge");

        let price_id =
            created_id(&post(&app, &seeded, project, &body("llama3.1:8b", 0.0, 0.0)).await);
        let rule = json!({
            "name": "Relevance on answers",
            "evaluator_id": judge,
            "target": "trace",
            "filter": "status = 'ok'",
            "variable_mapping": {"input": "trace.input", "output": "trace.output"},
            "sample_rate": 0.25,
            "budget_usd": 5.0,
            "budget_period": "day",
            "enabled": true
        });
        let created = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{project}/online-rules"),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&rule)
            .send()
            .await;
        assert_eq!(
            created.status,
            201,
            "{}",
            String::from_utf8_lossy(&created.body)
        );
        let rule_id = created.json::<Value>()["id"]
            .as_str()
            .expect("rule id")
            .to_owned();

        let url = format!("{}/{price_id}", path(project));
        let deleted = write_as(&app, &seeded, Method::DELETE, &url, None).await;
        assert_eq!(
            deleted.status,
            409,
            "{}",
            String::from_utf8_lossy(&deleted.body)
        );
        let errors = deleted.problem().errors;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, format!("rules.{rule_id}"));

        let output_only = json!({"model_name": "llama3.1:8b", "price_output": 0.2});
        let patched = write_as(&app, &seeded, Method::PATCH, &url, Some(&output_only)).await;
        assert_eq!(patched.status, 409);

        let both = json!({"model_name": "llama3.1:8b", "price_input": 0.1, "price_output": 0.2});
        let patched = write_as(&app, &seeded, Method::PATCH, &url, Some(&both)).await;
        assert_eq!(patched.status, 200);
    }

    #[tokio::test]
    async fn permissions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let project = seeded.project.id;

        let as_member = app
            .request(Method::POST, &path(project))
            .header(header::COOKIE.as_str(), &member.cookie)
            .json(&body("gpt-4o", 1.0, 1.0))
            .send()
            .await;
        assert_eq!(as_member.status, 403);

        let as_viewer = app
            .request(Method::GET, &path(project))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(as_viewer.status, 200);

        let read_key = app
            .request(
                Method::GET,
                &format!("{}/resolve?model=gpt-4o", path(project)),
            )
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .send()
            .await;
        assert_eq!(read_key.status, 200);

        let key_write = app
            .request(Method::POST, &path(project))
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.ingest_key),
            )
            .json(&body("gpt-4o", 1.0, 1.0))
            .send()
            .await;
        assert_eq!(key_write.status, 403);
    }

    /// One OTLP/JSON chat span of `trace` on `llama3.1:8b` with one million input tokens.
    fn chat_body(trace: u128) -> Vec<u8> {
        format!(
            "{{\"resourceSpans\":[{{\"scopeSpans\":[{{\"spans\":[{{\"traceId\":\"{trace:032x}\",\"spanId\":\"{trace:016x}\",\"name\":\"chat\",\"startTimeUnixNano\":\"1767225590000000000\",\"endTimeUnixNano\":\"1767225591000000000\",\"attributes\":[{{\"key\":\"gen_ai.operation.name\",\"value\":{{\"stringValue\":\"chat\"}}}},{{\"key\":\"gen_ai.request.model\",\"value\":{{\"stringValue\":\"llama3.1:8b\"}}}},{{\"key\":\"gen_ai.usage.input_tokens\",\"value\":{{\"intValue\":\"1000000\"}}}}]}}]}}]}}]}}"
        )
        .into_bytes()
    }

    /// Accepts one chat span through the ingest path, with the project's prices.
    async fn accept_chat(app: &TestApp, seeded: &Seeded, trace: u128) -> Span {
        let body = chat_body(trace);
        let reservation = app
            .state
            .ingest
            .reserve(Some(body.len() as u64))
            .expect("reserve");
        let accepted = app
            .state
            .ingest
            .accept(
                bytes::Bytes::from(body),
                reservation,
                BodyFormat::Json,
                BodyEncoding::Identity,
                &seeded.project,
                None,
                app.state.clock.now_ns(),
            )
            .await
            .expect("accepted");
        accepted
            .normalized
            .spans
            .into_iter()
            .next()
            .expect("one span")
    }

    /// The `cost_total` the span list shows for `span_id`.
    async fn listed_cost_total(app: &TestApp, seeded: &Seeded, span_id: &str) -> Value {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/spans", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let page: Value = response.json();
        page["data"]
            .as_array()
            .expect("data")
            .iter()
            .find(|span| span["span_id"] == span_id)
            .map(|span| span["cost_total"].clone())
            .expect("the span is listed")
    }

    #[tokio::test]
    async fn a_new_price_applies_to_spans_ingested_after_it() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let price = created_id(
            &post(
                &app,
                &seeded,
                seeded.project.id,
                &body("llama3.1:8b", 1.0, 1.0),
            )
            .await,
        );
        // 1 000 000 input tokens at 1.0 USD per 1M.
        let before = accept_chat(&app, &seeded, 1).await;
        assert_eq!(before.cost_total, Some(1.0));
        let span_id = before.span_id.to_string();
        app.commit_spans(&seeded.project, vec![before]).await;
        assert_eq!(listed_cost_total(&app, &seeded, &span_id).await, json!(1.0));

        let url = format!("{}/{price}", path(seeded.project.id));
        let patched = write_as(
            &app,
            &seeded,
            Method::PATCH,
            &url,
            Some(&body("llama3.1:8b", 3.0, 3.0)),
        )
        .await;
        assert_eq!(
            patched.status,
            200,
            "{}",
            String::from_utf8_lossy(&patched.body)
        );

        let after = accept_chat(&app, &seeded, 2).await;
        assert_eq!(after.cost_total, Some(3.0));
        // D-T-5: the stored span keeps the cost it was priced with.
        assert_eq!(listed_cost_total(&app, &seeded, &span_id).await, json!(1.0));
    }

    #[tokio::test]
    async fn a_write_does_not_fail_on_a_rule_it_did_not_break() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (org, project, now) = (seeded.org_id, seeded.project.id, app.state.clock.now_ns());
        let (connection, judge, rule) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        // A budgeted rule whose model has no price. The route refuses to create it, so the row
        // is written here, below the route.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_connection(
                        conn,
                        connection,
                        org,
                        ConnectionFields {
                            name: format!("judge {connection}"),
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
                        },
                        None,
                        now,
                    )
                    .await?;
                    insert_evaluator(
                        conn,
                        judge,
                        project,
                        "llm_judge",
                        EvaluatorFields {
                            name: format!("judge {judge}"),
                            config: json!({
                                "connection_id": connection,
                                "model": "no-such-model-9",
                                "prompt": "{{ input }} {{ output }}",
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
                    insert_rule(
                        conn,
                        NewRule {
                            id: rule,
                            project_id: project,
                            evaluator_id: judge,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        RuleFields {
                            name: "Unpriced".to_owned(),
                            filter: None,
                            variable_mapping: r#"{"input":"trace.input","output":"trace.output"}"#
                                .to_owned(),
                            sample_rate: 0.5,
                            budget_usd: Some(5.0),
                            budget_period: Some("day".to_owned()),
                            enabled: true,
                        },
                        now,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .expect("the unpriced rule is stored");

        let response = post(&app, &seeded, project, &body("unrelated-model", 1.0, 1.0)).await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
    }

    #[tokio::test]
    async fn writes_are_audited() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let probe = "audit-probe-model-x";
        let (created_body, logs) = capture_logs(async {
            let created = post(&app, &seeded, project, &body(probe, 1.0, 1.0)).await;
            let id = created_id(&created);
            let url = format!("{}/{id}", path(project));
            let replaced = json!({"model_name": probe, "price_input": 2.0, "price_output": 2.0});
            let patched = write_as(&app, &seeded, Method::PATCH, &url, Some(&replaced)).await;
            assert_eq!(patched.status, 200);
            let deleted = write_as(&app, &seeded, Method::DELETE, &url, None).await;
            assert_eq!(deleted.status, 204);
            id
        })
        .await;
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT action, target_id, project_id FROM audit_events \
             WHERE action LIKE 'model_price.%' ORDER BY seq",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        let actions: Vec<&str> = rows.iter().map(|(action, _, _)| action.as_str()).collect();
        assert_eq!(
            actions,
            ["model_price.created", "model_price.updated", "model_price.deleted"]
        );
        for (_, target, scope) in &rows {
            assert_eq!(target, &created_body);
            assert_eq!(scope.as_deref(), Some(project.to_string().as_str()));
        }
        assert!(!logs.contains(probe), "{logs}");
    }

    #[tokio::test]
    async fn org_price_write_is_recorded_without_project() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = call(
            &app,
            &seeded.cookie,
            Method::POST,
            ORG,
            Some(&body("org-audit-model", 1.0, 1.0)),
        )
        .await;
        assert_eq!(created.status, 201);
        let id = created_id(&created);
        let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT action, org_id, project_id, target_id FROM audit_events \
             WHERE action LIKE 'model_price.%'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(
                "model_price.created".to_owned(),
                Some(seeded.org_id.to_string()),
                None,
                Some(id),
            )]
        );
    }

    #[tokio::test]
    async fn the_501st_project_row_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (org, project, now) = (seeded.org_id, seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for index in 0..500 {
                        let name = format!("cap-{index}");
                        let row = PriceRow {
                            scope: PriceScope::Project,
                            model_name: name.clone(),
                            match_pattern: exact_pattern(&name),
                            provider: None,
                            valid_from: 0,
                            price_input: Some(1.0),
                            price_output: Some(1.0),
                            price_cache_read: None,
                            price_cache_write: None,
                            price_reasoning: None,
                            tiers: None,
                        };
                        model_prices::insert_tx(
                            conn,
                            Uuid::now_v7(),
                            org,
                            Some(project),
                            &row,
                            now,
                        )
                        .await?;
                    }
                    Ok(())
                })
            })
            .await
            .expect("500 rows");
        let response = post(&app, &seeded, project, &body("over-cap", 1.0, 1.0)).await;
        assert_eq!(response.status, 409);
    }

    /// The org's price path of `acme` (the seeded org).
    const ORG: &str = "/api/v1/orgs/acme/models";

    async fn call(
        app: &TestApp,
        cookie: &str,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> TestResponse {
        let request = app
            .request(method, url)
            .header(header::COOKIE.as_str(), cookie);
        match body {
            Some(body) => request.json(body).send().await,
            None => request.send().await,
        }
    }

    async fn project_get(app: &TestApp, cookie: &str, project: ProjectId, query: &str) -> Value {
        call(
            app,
            cookie,
            Method::GET,
            &format!("{}{query}", path(project)),
            None,
        )
        .await
        .json()
    }

    /// A user with `role` in `org`, signed in: the session cookie.
    async fn member_cookie(app: &TestApp, org: OrgId, email: &str, role: OrgRole) -> String {
        let now = app.state.clock.now_ns();
        let email = email.to_owned();
        let user = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
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
                    .await?;
                    add_org_member(conn, org, user.id, role, now).await?;
                    Ok(user)
                })
            })
            .await
            .expect("member");
        session_cookie_via(app, user.id, None).await
    }

    /// A budgeted judge rule in `project`, with its judge's connection, and the rule id. Its
    /// judge model needs a price when the rule is created (D-I-15).
    async fn budgeted_rule(app: &TestApp, seeded: &Seeded, project: ProjectId) -> String {
        let (org, now) = (seeded.org_id, app.state.clock.now_ns());
        let judge = Uuid::now_v7();
        let connection = Uuid::now_v7();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_connection(
                        conn,
                        connection,
                        org,
                        ConnectionFields {
                            name: format!("judge {connection}"),
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
                        },
                        None,
                        now,
                    )
                    .await?;
                    insert_evaluator(
                        conn,
                        judge,
                        project,
                        "llm_judge",
                        EvaluatorFields {
                            name: format!("judge {judge}"),
                            config: json!({
                                "connection_id": connection,
                                "model": "llama3.1:8b",
                                "prompt": "{{ input }} {{ output }}",
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
                    Ok(())
                })
            })
            .await
            .expect("connection and judge");
        let rule = json!({
            "name": "Relevance on answers",
            "evaluator_id": judge,
            "target": "trace",
            "filter": "status = 'ok'",
            "variable_mapping": {"input": "trace.input", "output": "trace.output"},
            "sample_rate": 0.25,
            "budget_usd": 5.0,
            "budget_period": "day",
            "enabled": true
        });
        let created = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{project}/online-rules"),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&rule)
            .send()
            .await;
        assert_eq!(
            created.status,
            201,
            "{}",
            String::from_utf8_lossy(&created.body)
        );
        created.json::<Value>()["id"]
            .as_str()
            .expect("rule id")
            .to_owned()
    }

    #[tokio::test]
    async fn org_round_trip() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let cookie = seeded.cookie.clone();

        let created = call(
            &app,
            &cookie,
            Method::POST,
            ORG,
            Some(&body("llama3.1:8b", 0.0, 0.0)),
        )
        .await;
        let id = created_id(&created);
        assert_eq!(created.json::<Value>()["scope"], "org");

        let page: Value = call(&app, &cookie, Method::GET, ORG, None).await.json();
        assert_eq!(page["data"].as_array().map(Vec::len), Some(1));
        assert_eq!(page["next_cursor"], Value::Null);

        let url = format!("{ORG}/{id}");
        let replaced = body("llama3.1:8b", 0.1, 0.2);
        let patched = call(&app, &cookie, Method::PATCH, &url, Some(&replaced)).await;
        assert_eq!(
            patched.status,
            200,
            "{}",
            String::from_utf8_lossy(&patched.body)
        );
        assert_eq!(patched.json::<Value>()["price_input"], 0.1);

        let deleted = call(&app, &cookie, Method::DELETE, &url, None).await;
        assert_eq!(deleted.status, 204);
        let page: Value = call(&app, &cookie, Method::GET, ORG, None).await.json();
        assert_eq!(page["data"], json!([]));
    }

    #[tokio::test]
    async fn an_org_row_reaches_every_project() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let cookie = seeded.cookie.clone();
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let id = created_id(
            &call(
                &app,
                &cookie,
                Method::POST,
                ORG,
                Some(&body("llama3.1:8b", 0.0, 0.0)),
            )
            .await,
        );

        for project in [seeded.project.id, other.id] {
            let page = project_get(&app, &cookie, project, "?scope=org").await;
            assert_eq!(page["data"].as_array().map(Vec::len), Some(1));
            let found = project_get(&app, &cookie, project, "/resolve?model=llama3.1:8b").await;
            assert_eq!(found["price"]["id"], id);
            assert_eq!(found["price"]["scope"], "org");
        }

        // A project of another org never sees these rows.
        let now = app.state.clock.now_ns();
        let (foreign_org, foreign_project) = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let org = insert_org(conn, OrgId::new_v7(), "Other", "other-org", now).await?;
                    let project =
                        insert_project(conn, org.id, "Other", "other-project", None, now).await?;
                    Ok((org.id, project.id))
                })
            })
            .await
            .expect("another org");
        let table = app
            .state
            .ingest
            .prices()
            .for_project(foreign_org, foreign_project);
        assert!(
            table
                .price_for("llama3.1:8b", None, now)
                .is_none_or(|row| row.scope != PriceScope::Org)
        );
    }

    #[tokio::test]
    async fn a_project_row_beats_the_org_row() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let cookie = seeded.cookie.clone();
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let org_id = created_id(
            &call(
                &app,
                &cookie,
                Method::POST,
                ORG,
                Some(&body("llama3.1:8b", 1.0, 1.0)),
            )
            .await,
        );
        let project_id = created_id(
            &post(
                &app,
                &seeded,
                seeded.project.id,
                &body("llama3.1:8b", 2.0, 2.0),
            )
            .await,
        );

        let mine = project_get(
            &app,
            &cookie,
            seeded.project.id,
            "/resolve?model=llama3.1:8b",
        )
        .await;
        assert_eq!(mine["price"]["id"], project_id);
        assert_eq!(mine["price"]["scope"], "project");

        let theirs = project_get(&app, &cookie, other.id, "/resolve?model=llama3.1:8b").await;
        assert_eq!(theirs["price"]["id"], org_id);
        assert_eq!(theirs["price"]["scope"], "org");
    }

    #[tokio::test]
    async fn org_writes_guard_rules_in_every_project() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let cookie = seeded.cookie.clone();
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let org_price = created_id(
            &call(
                &app,
                &cookie,
                Method::POST,
                ORG,
                Some(&body("llama3.1:8b", 0.0, 0.0)),
            )
            .await,
        );
        let rule_id = budgeted_rule(&app, &seeded, other.id).await;

        let url = format!("{ORG}/{org_price}");
        let deleted = call(&app, &cookie, Method::DELETE, &url, None).await;
        assert_eq!(
            deleted.status,
            409,
            "{}",
            String::from_utf8_lossy(&deleted.body)
        );
        let errors = deleted.problem().errors;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, format!("rules.{rule_id}"));

        // A project row prices the model in that project, so the org row can go.
        created_id(&post(&app, &seeded, other.id, &body("llama3.1:8b", 0.0, 0.0)).await);
        let deleted = call(&app, &cookie, Method::DELETE, &url, None).await;
        assert_eq!(
            deleted.status,
            204,
            "{}",
            String::from_utf8_lossy(&deleted.body)
        );
    }

    #[tokio::test]
    async fn org_permissions_and_scopes() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let cookie = seeded.cookie.clone();
        let org = seeded.org_id;
        let price = created_id(
            &call(
                &app,
                &cookie,
                Method::POST,
                ORG,
                Some(&body("llama3.1:8b", 0.0, 0.0)),
            )
            .await,
        );
        let url = format!("{ORG}/{price}");
        let viewer = member_cookie(&app, org, "viewer@example.com", OrgRole::Viewer).await;
        let member = member_cookie(&app, org, "member@example.com", OrgRole::Member).await;
        let admin = member_cookie(&app, org, "admin@example.com", OrgRole::Admin).await;
        let new_row = body("gpt-4o", 1.0, 1.0);

        for reader in [&viewer, &member] {
            let listed = call(&app, reader, Method::GET, ORG, None).await;
            assert_eq!(listed.status, 200);
            let created = call(&app, reader, Method::POST, ORG, Some(&new_row)).await;
            assert_eq!(created.status, 403);
            let patched = call(&app, reader, Method::PATCH, &url, Some(&new_row)).await;
            assert_eq!(patched.status, 403);
            let deleted = call(&app, reader, Method::DELETE, &url, None).await;
            assert_eq!(deleted.status, 403);
        }

        let created = call(&app, &admin, Method::POST, ORG, Some(&new_row)).await;
        assert_eq!(created.status, 201);
        let extra = created_id(&created);
        let deleted = call(
            &app,
            &admin,
            Method::DELETE,
            &format!("{ORG}/{extra}"),
            None,
        )
        .await;
        assert_eq!(deleted.status, 204);

        // A row of another org is not found through this org.
        let other = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &cookie)
            .json(&json!({"name": "Elsewhere"}))
            .send()
            .await;
        assert_eq!(
            other.status,
            201,
            "{}",
            String::from_utf8_lossy(&other.body)
        );
        let slug = other.json::<Value>()["slug"]
            .as_str()
            .expect("slug")
            .to_owned();
        let foreign = created_id(
            &call(
                &app,
                &cookie,
                Method::POST,
                &format!("/api/v1/orgs/{slug}/models"),
                Some(&body("llama3.1:8b", 0.0, 0.0)),
            )
            .await,
        );
        let patched = call(
            &app,
            &cookie,
            Method::PATCH,
            &format!("{ORG}/{foreign}"),
            Some(&new_row),
        )
        .await;
        assert_eq!(patched.status, 404);

        // Scopes do not cross: a project row is not an org row, and the reverse.
        let project_row = created_id(&post(&app, &seeded, seeded.project.id, &new_row).await);
        let patched = call(
            &app,
            &cookie,
            Method::PATCH,
            &format!("{ORG}/{project_row}"),
            Some(&new_row),
        )
        .await;
        assert_eq!(patched.status, 404);
        let patched = call(
            &app,
            &cookie,
            Method::PATCH,
            &format!("{}/{price}", path(seeded.project.id)),
            Some(&new_row),
        )
        .await;
        assert_eq!(patched.status, 404);
    }

    #[tokio::test]
    async fn the_501st_org_row_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let cookie = seeded.cookie.clone();
        for index in 0..500 {
            let created = call(
                &app,
                &cookie,
                Method::POST,
                ORG,
                Some(&body(&format!("org-cap-{index}"), 0.0, 0.0)),
            )
            .await;
            assert_eq!(created.status, 201, "row {index}");
        }
        let response = call(
            &app,
            &cookie,
            Method::POST,
            ORG,
            Some(&body("over-cap", 1.0, 1.0)),
        )
        .await;
        assert_eq!(response.status, 409);
    }
}
