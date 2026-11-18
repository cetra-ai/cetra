//! Custom dashboards (spec 09, spec 04 § Dashboards, D-C-16).
//!
//! Reads need project view and keys read them. Writes need [`ProjectPerm::ManageDashboards`]:
//! member and above through a session or a read-write personal access token. An API key cannot
//! write (the access layer answers 403).

use std::collections::HashSet;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use cetra_core::ids::ProjectId;
use cetra_core::time_serde::parse_rfc3339;
use cetra_storage as storage;

use crate::api::common::{FieldError, Page};
use crate::api::convert;
use crate::api::dashboards::{
    ChartType, CreateDashboard, Dashboard, DashboardListParams, DashboardSummary, Definition,
    UpdateDashboard, Widget, WidgetKind, WidgetQueryBody, WidgetQueryRequest, WidgetQueryResponse,
    WidgetShape,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::dashboards::{self as store, DashboardChanges, NewDashboard, UpdateOutcome};
use crate::routes::metrics::{ProjectKey, SharedError, bucket, storage_group_by};
use crate::routes::read::{READ_TIMEOUT, open_query, page_limit};

/// A 256 KiB definition plus the rest of the body fits (spec 04).
const DASHBOARD_BODY_LIMIT: usize = 300 * 1024;
/// Longest serialized `definition`, in bytes (spec 04).
const MAX_DEFINITION_BYTES: usize = 256 * 1024;
/// Most widgets in one dashboard (spec 04).
const MAX_WIDGETS: usize = 50;
/// Longest name and widget title, in characters after trimming.
const MAX_NAME_CHARS: usize = 100;
/// Longest description, in characters.
const MAX_DESCRIPTION_CHARS: usize = 500;
/// Longest widget id, in characters.
const MAX_ID_CHARS: usize = 64;
/// Longest widget title, in characters.
const MAX_TITLE_CHARS: usize = 100;
/// Longest measure label, in characters.
const MAX_LABEL_CHARS: usize = 60;
/// Most groups a split chart draws (spec 04).
const MAX_SPLIT_GROUPS: usize = 6;

/// Dashboard routes (spec 09).
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/dashboards",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_dashboards),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/dashboards",
            access: Access::Project {
                perm: ProjectPerm::ManageDashboards,
                session_only: false,
            },
            documented: true,
            handler: post(create_dashboard).layer(DefaultBodyLimit::max(DASHBOARD_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/dashboards/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_dashboard),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/dashboards/query",
            access: Access::Project {
                perm: ProjectPerm::Query,
                session_only: false,
            },
            documented: true,
            handler: post(query_widget),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/dashboards/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageDashboards,
                session_only: false,
            },
            documented: true,
            handler: patch(update_dashboard).layer(DefaultBodyLimit::max(DASHBOARD_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/dashboards/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageDashboards,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_dashboard),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct DashboardPath {
    id: Uuid,
}

/// The key of a widget answer: everything that changes it (spec 05 § Dashboard queries).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WidgetKey {
    project: ProjectId,
    /// The org's data window in days (D-P-21).
    data_access_days: Option<u64>,
    /// The request body as JSON. The field order is the struct's, so equal bodies give equal text.
    body: String,
}

impl ProjectKey for WidgetKey {
    fn project(&self) -> ProjectId {
        self.project
    }
}

async fn list_dashboards(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<DashboardListParams>,
) -> Result<error::Json<Page<DashboardSummary>>, ApiError> {
    let after = params.cursor.as_deref().map(decode_cursor).transpose()?;
    let page = store::list(
        state.meta.reader(),
        access.project.id,
        page_limit(params.limit),
        after.as_ref().map(|(name, id)| (name.as_str(), *id)),
    )
    .await?;
    let next_cursor = page
        .next
        .as_ref()
        .map(|(name, id)| encode_cursor(name, *id))
        .transpose()?;
    Ok(error::Json(Page {
        data: page.rows.into_iter().map(DashboardSummary::from).collect(),
        next_cursor,
    }))
}

async fn get_dashboard(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<DashboardPath>,
) -> Result<error::Json<Dashboard>, ApiError> {
    store::get(state.meta.reader(), access.project.id, path.id)
        .await?
        .map(|row| error::Json(Dashboard::from(row)))
        .ok_or(ApiError::NotFound)
}

async fn create_dashboard(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<CreateDashboard>,
) -> Result<Response, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let project = access.project.id;
    let mut errors = Vec::new();
    let name = checked_name(&body.name, "name", &mut errors);
    // A blank description is no description (PATCH clears with "", spec 09).
    let description = checked_description(body.description.as_deref(), "description", &mut errors)
        .filter(|text| !text.is_empty());
    check_definition(&body.definition, "definition", &mut errors);
    refuse_if_any(errors)?;
    check_expressions(&state, &access, &body.definition, "definition").await?;

    let now = state.clock.now_ns();
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::insert(
                    conn,
                    NewDashboard {
                        project_id: project,
                        name: name.unwrap_or_default(),
                        description,
                        definition: body.definition,
                        user,
                    },
                    now,
                )
                .await
            })
        })
        .await?;
    tracing::info!(project_id = %project, dashboard_id = %row.info.id, "dashboard created");
    Ok((StatusCode::CREATED, error::Json(Dashboard::from(row))).into_response())
}

async fn update_dashboard(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<DashboardPath>,
    error::Json(body): error::Json<UpdateDashboard>,
) -> Result<error::Json<Dashboard>, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let project = access.project.id;
    let mut errors = Vec::new();
    let name = body
        .name
        .as_deref()
        .map(|name| checked_name(name, "name", &mut errors))
        .unwrap_or_default();
    // Omitted: unchanged. `""`: cleared (stored as NULL). Other text: replaced.
    let description = body.description.as_deref().map(|text| {
        checked_description(Some(text), "description", &mut errors).filter(|text| !text.is_empty())
    });
    if let Some(definition) = &body.definition {
        check_definition(definition, "definition", &mut errors);
    }
    refuse_if_any(errors)?;
    if let Some(definition) = &body.definition {
        check_expressions(&state, &access, definition, "definition").await?;
    }

    let changes = DashboardChanges {
        name,
        description,
        definition: body.definition,
    };
    let version = body.version;
    let id = path.id;
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { store::update(conn, project, id, version, changes, user, now).await },
            )
        })
        .await?;
    match outcome {
        UpdateOutcome::Updated(row) => {
            tracing::info!(project_id = %project, dashboard_id = %id, "dashboard updated");
            Ok(error::Json(Dashboard::from(row)))
        }
        UpdateOutcome::Stale(current) => Err(ApiError::StaleVersion { current }),
        UpdateOutcome::Missing => Err(ApiError::NotFound),
    }
}

async fn delete_dashboard(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<DashboardPath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    let id = path.id;
    let deleted = state
        .meta
        .write(move |conn| Box::pin(async move { store::delete(conn, project, id).await }))
        .await?;
    if deleted {
        tracing::info!(project_id = %project, dashboard_id = %id, "dashboard deleted");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// The position a cursor names: the `(name, id)` of the previous page's last dashboard.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardCursor {
    name: String,
    id: Uuid,
}

/// The opaque cursor of a page, URL-safe base64 of the JSON position (as alert events do).
fn encode_cursor(name: &str, id: Uuid) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(&DashboardCursor {
        name: name.to_owned(),
        id,
    })
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The position of a cursor. Text that does not decode is `400` on `cursor`.
fn decode_cursor(text: &str) -> Result<(String, Uuid), ApiError> {
    let invalid = || field_error("cursor", "invalid cursor");
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
    let cursor: DashboardCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    Ok((cursor.name, cursor.id))
}

/// Answers every collected error at once as `400 bad_request`.
fn refuse_if_any(errors: Vec<FieldError>) -> Result<(), ApiError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::BadRequest {
            detail: None,
            errors,
        })
    }
}

fn field(path: impl Into<String>, message: &str) -> FieldError {
    FieldError {
        field: path.into(),
        message: message.to_owned(),
        position: None,
    }
}

/// Trimmed, 1 to 100 characters. Returns the trimmed name when it is valid.
fn checked_name(name: &str, path: &str, errors: &mut Vec<FieldError>) -> Option<String> {
    let trimmed = name.trim();
    let length = trimmed.chars().count();
    if (1..=MAX_NAME_CHARS).contains(&length) {
        Some(trimmed.to_owned())
    } else {
        errors.push(field(path, "must be 1 to 100 characters"));
        None
    }
}

/// 0 to 500 characters after trimming. Returns the trimmed text when it is valid; blank text
/// comes back empty, which the callers store as none.
fn checked_description(
    text: Option<&str>,
    path: &str,
    errors: &mut Vec<FieldError>,
) -> Option<String> {
    let text = text?.trim();
    if text.chars().count() <= MAX_DESCRIPTION_CHARS {
        Some(text.to_owned())
    } else {
        errors.push(field(path, "must be at most 500 characters"));
        None
    }
}

/// Checks a whole definition: its size, its widget count, and every widget's rules. Errors are
/// appended with paths under `path`. Expressions are checked later, once the rest passed.
fn check_definition(definition: &Definition, path: &str, errors: &mut Vec<FieldError>) {
    match serde_json::to_vec(definition) {
        Ok(bytes) if bytes.len() <= MAX_DEFINITION_BYTES => {}
        _ => errors.push(field(path, "must be at most 262144 bytes")),
    }
    if definition.widgets.len() > MAX_WIDGETS {
        errors.push(field(
            format!("{path}.widgets"),
            "must have at most 50 widgets",
        ));
    }
    let mut ids = HashSet::new();
    for (index, widget) in definition.widgets.iter().enumerate() {
        check_widget(
            widget,
            &format!("{path}.widgets[{index}]"),
            &mut ids,
            errors,
        );
    }
}

fn check_widget(
    widget: &Widget,
    base: &str,
    ids: &mut HashSet<String>,
    errors: &mut Vec<FieldError>,
) {
    let id_ok = (1..=MAX_ID_CHARS).contains(&widget.id.chars().count())
        && widget
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !id_ok {
        errors.push(field(
            format!("{base}.id"),
            "must be 1 to 64 characters of letters, digits, _ or -",
        ));
    } else if !ids.insert(widget.id.clone()) {
        errors.push(field(
            format!("{base}.id"),
            "must be unique in the dashboard",
        ));
    }

    let title_min = match widget.kind {
        WidgetKind::Chart => 0,
        WidgetKind::Heading => 1,
    };
    // A heading must hold text after trimming; a chart title keeps its raw length.
    let title_length = match widget.kind {
        WidgetKind::Chart => widget.title.chars().count(),
        WidgetKind::Heading => widget.title.trim().chars().count(),
    };
    if !(title_min..=MAX_TITLE_CHARS).contains(&title_length) {
        errors.push(field(
            format!("{base}.title"),
            if title_min == 1 {
                "a heading needs 1 to 100 characters"
            } else {
                "must be at most 100 characters"
            },
        ));
    }

    let layout = widget.layout;
    if !(0..=11).contains(&layout.x) {
        errors.push(field(format!("{base}.layout.x"), "must be 0 to 11"));
    }
    if !(0..=1000).contains(&layout.y) {
        errors.push(field(format!("{base}.layout.y"), "must be 0 to 1000"));
    }
    if !(1..=12).contains(&layout.w) {
        errors.push(field(format!("{base}.layout.w"), "must be 1 to 12"));
    } else if layout.x.saturating_add(layout.w) > 12 {
        errors.push(field(
            format!("{base}.layout.w"),
            "x + w must be at most 12",
        ));
    }
    if !(1..=40).contains(&layout.h) {
        errors.push(field(format!("{base}.layout.h"), "must be 1 to 40"));
    }

    match widget.kind {
        WidgetKind::Heading => {
            if widget.chart.is_some() {
                errors.push(field(format!("{base}.chart"), "a heading has no chart"));
            }
            if widget.query.is_some() {
                errors.push(field(format!("{base}.query"), "a heading has no query"));
            }
        }
        WidgetKind::Chart => {
            match &widget.chart {
                None => errors.push(field(format!("{base}.chart"), "a chart needs chart")),
                Some(chart) => {
                    check_chart_type(chart.chart_type, chart, base, errors);
                }
            }
            match &widget.query {
                None => errors.push(field(format!("{base}.query"), "a chart needs a query")),
                Some(query) => check_query(query, widget, base, errors),
            }
        }
    }
}

/// The per-type rules of a chart's settings.
fn check_chart_type(
    chart_type: ChartType,
    chart: &crate::api::dashboards::Chart,
    base: &str,
    errors: &mut Vec<FieldError>,
) {
    if chart.stacked.is_some() && !matches!(chart_type, ChartType::Bar | ChartType::Area) {
        errors.push(field(
            format!("{base}.chart.stacked"),
            "only bar and area charts stack",
        ));
    }
    if chart.split.is_some() && chart_type != ChartType::Line {
        errors.push(field(
            format!("{base}.chart.split"),
            "only line charts split",
        ));
    }
    if let Some(fade) = chart.fade_below {
        if chart_type != ChartType::Bar {
            errors.push(field(
                format!("{base}.chart.fade_below"),
                "only bar charts fade",
            ));
        } else if !(1..=1000).contains(&fade) {
            errors.push(field(
                format!("{base}.chart.fade_below"),
                "must be 1 to 1000",
            ));
        }
    }
    if chart.share.is_some() && chart_type != ChartType::Table {
        errors.push(field(
            format!("{base}.chart.share"),
            "only table charts show a share",
        ));
    }
}

/// The query's shape, its labels, and the chart-type rules that read the query.
fn check_query(query: &WidgetQueryBody, widget: &Widget, base: &str, errors: &mut Vec<FieldError>) {
    let query_base = format!("{base}.query");
    let storage = query.to_storage();
    if let Err(cetra_storage::WidgetError::Fields(found)) = cetra_storage::validate_shape(&storage)
    {
        for error in found {
            errors.push(field(
                format!("{query_base}.{}", error.path),
                &error.message,
            ));
        }
    }
    for (index, measure) in query.measures.iter().enumerate() {
        if let Some(label) = &measure.label
            && label.chars().count() > MAX_LABEL_CHARS
        {
            errors.push(field(
                format!("{query_base}.measures[{index}].label"),
                "must be at most 60 characters",
            ));
        }
    }
    let Some(chart) = &widget.chart else {
        return;
    };
    // A split line chart draws one small chart per group, so it needs a breakdown of at most 6.
    if chart.chart_type == ChartType::Line && chart.split == Some(true) {
        match &query.breakdown {
            None => errors.push(field(
                format!("{query_base}.breakdown"),
                "a split chart needs a breakdown",
            )),
            Some(breakdown)
                if breakdown
                    .limit
                    .unwrap_or(cetra_storage::DEFAULT_BREAKDOWN_LIMIT)
                    > MAX_SPLIT_GROUPS =>
            {
                errors.push(field(
                    format!("{query_base}.breakdown.limit"),
                    "a split chart has at most 6 groups",
                ));
            }
            Some(_) => {}
        }
    }
    match chart.chart_type {
        ChartType::BarH if query.breakdown.is_none() => errors.push(field(
            format!("{query_base}.breakdown"),
            "a horizontal bar chart needs a breakdown",
        )),
        ChartType::Number => {
            if query.measures.len() != 1 {
                errors.push(field(
                    format!("{query_base}.measures"),
                    "a number chart has exactly one measure",
                ));
            }
            if query.breakdown.is_some() {
                errors.push(field(
                    format!("{query_base}.breakdown"),
                    "a number chart has no breakdown",
                ));
            }
        }
        _ => {}
    }
}

/// Parses every chart widget's expressions against their views (spec 05). Runs only when the
/// shape checks passed. Stops at the first invalid expression.
async fn check_expressions(
    state: &AppState,
    access: &ProjectAccess,
    definition: &Definition,
    path: &str,
) -> Result<(), ApiError> {
    let queried = definition
        .widgets
        .iter()
        .enumerate()
        .filter_map(|(index, widget)| widget.query.as_ref().map(|query| (index, query)))
        .collect::<Vec<_>>();
    if queried.is_empty() {
        return Ok(());
    }
    let ctx = open_query(state, access.project.org_id, access.project.id, None).await?;
    for (index, query) in queried {
        match cetra_storage::validate_expressions(&ctx, &query.to_storage(), None) {
            Ok(()) => {}
            Err(cetra_storage::WidgetError::Expression { path: inner, error }) => {
                return Err(ApiError::QueryAt {
                    field: format!("{path}.widgets[{index}].query.{inner}"),
                    error: cetra_storage::RunError::Invalid(error),
                });
            }
            Err(cetra_storage::WidgetError::Run(error)) => return Err(error.into()),
            Err(cetra_storage::WidgetError::Fields(found)) => {
                let errors = found
                    .into_iter()
                    .map(|error| {
                        field(
                            format!("{path}.widgets[{index}].query.{}", error.path),
                            &error.message,
                        )
                    })
                    .collect();
                return Err(ApiError::BadRequest {
                    detail: None,
                    errors,
                });
            }
        }
    }
    Ok(())
}

/// `POST /dashboards/query`: one widget's data (spec 05 § Dashboard queries). A read, so a
/// read-only PAT and a `read` key may call it.
async fn query_widget(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<WidgetQueryRequest>,
) -> Result<error::Json<WidgetQueryResponse>, SharedError> {
    let (from, to) = query_range(&body)?;
    let key = WidgetKey {
        project: access.project.id,
        data_access_days: crate::entitlements::of(&state, access.project.org_id)
            .limits
            .data_access_days,
        body: serde_json::to_string(&body).map_err(|error| ApiError::Internal(Box::new(error)))?,
    };
    let (org, project) = (access.project.org_id, access.project.id);
    let clock = &state.clock;
    let response = state
        .widget_cache
        .get_or_compute(
            key,
            || clock.now_ns(),
            || async {
                let window = storage::TimeWindow::new(from, to)?;
                let ctx = open_query(&state, org, project, None).await?;
                let query = body.query.to_storage();
                let data = storage::widget_query(
                    &ctx,
                    storage::WidgetRequest {
                        window,
                        bucket: bucket(body.bucket),
                        dashboard_filter: body.filter.as_deref(),
                        group_by: body.group_by.map(storage_group_by),
                        include_eval: body.include_eval,
                        include_experiments: body.include_experiments,
                        query: &query,
                        shape: storage_shape(body.shape),
                        compare: body.compare,
                        timeout: READ_TIMEOUT,
                    },
                )
                .await
                .map_err(widget_error)?;
                Ok(convert::widget_response(data))
            },
        )
        .await
        .map_err(SharedError)?;
    Ok(error::Json((*response).clone()))
}

/// The request's `[from, to)` as nanoseconds. Both are RFC 3339 and `from` is before `to`.
fn query_range(body: &WidgetQueryRequest) -> Result<(i64, i64), ApiError> {
    const RFC3339: &str = "expected an RFC 3339 timestamp";
    let from = parse_rfc3339(&body.from).map_err(|_| field_error("from", RFC3339))?;
    let to = parse_rfc3339(&body.to).map_err(|_| field_error("to", RFC3339))?;
    if from >= to {
        return Err(field_error("from", "from must be before to"));
    }
    Ok((from, to))
}

fn storage_shape(shape: WidgetShape) -> storage::Shape {
    match shape {
        WidgetShape::Series => storage::Shape::Series,
        WidgetShape::Total => storage::Shape::Total,
    }
}

/// The problem of a refused widget query. Field paths are the request's: the widget's query
/// fields sit under `query`, and the dashboard's `filter` is the request's own `filter`.
fn widget_error(error: storage::WidgetError) -> ApiError {
    match error {
        storage::WidgetError::Fields(found) => ApiError::BadRequest {
            detail: None,
            errors: found
                .into_iter()
                .map(|error| field(format!("query.{}", error.path), &error.message))
                .collect(),
        },
        storage::WidgetError::Expression { path, error } => ApiError::QueryAt {
            field: expression_field(&path),
            error: storage::RunError::Invalid(error),
        },
        storage::WidgetError::Run(error) => ApiError::Query(error),
    }
}

/// The request path of an invalid expression that the storage layer names.
fn expression_field(path: &str) -> String {
    if path == "dashboard_filter" {
        "filter".to_owned()
    } else {
        format!("query.{path}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::{Value, json};

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::auth::roles::OrgRole;
    use crate::meta::projects::delete_project;
    use crate::test_support::{Seeded, TestApp, set_limits};
    use cetra_core::ids::{ProjectId, TraceId};
    use std::str::FromStr;

    fn heading(id: &str) -> Value {
        json!({
            "id": id, "title": "Traffic",
            "layout": {"x": 0, "y": 0, "w": 12, "h": 1},
            "kind": "heading"
        })
    }

    // `json!` borrows its arguments, so these helpers look unconsumed to clippy.
    #[allow(clippy::needless_pass_by_value)]
    fn query(measures: Value) -> Value {
        json!({"source": "traces", "measures": measures})
    }

    /// A chart widget with `chart` settings and `query`, at the top left.
    #[allow(clippy::needless_pass_by_value)]
    fn chart(chart: Value, query: Value) -> Value {
        json!({
            "id": "w", "title": "Chart",
            "layout": {"x": 0, "y": 0, "w": 6, "h": 4},
            "kind": "chart", "chart": chart, "query": query
        })
    }

    fn valid_definition() -> Value {
        let mut value = valid_widgets();
        for (index, widget) in value["widgets"]
            .as_array_mut()
            .expect("widgets is an array")
            .iter_mut()
            .enumerate()
        {
            widget["id"] = json!(format!("w{index}"));
        }
        value
    }

    fn valid_widgets() -> Value {
        json!({"widgets": [
            heading("heading-traffic"),
            chart(
                json!({"type": "line"}),
                json!({
                    "source": "traces",
                    "measures": [{"agg": "count", "label": "Requests"}],
                    "breakdown": {"field": "models", "limit": 5}
                })
            ),
            chart(
                json!({"type": "number"}),
                query(json!([{"agg": "rate", "where": "error_count > 0", "label": "Errors"}]))
            ),
            chart(
                json!({"type": "table", "share": true}),
                json!({
                    "source": "traces",
                    "measures": [{"agg": "p95", "field": "duration_ns"}],
                    "breakdown": {"field": "name", "limit": 10}
                })
            ),
        ]})
    }

    fn definition(value: Value) -> Definition {
        serde_json::from_value(value).expect("test definition parses")
    }

    /// The field paths of every error `check_definition` finds.
    fn paths_of(value: Value) -> Vec<String> {
        let mut errors = Vec::new();
        check_definition(&definition(value), "definition", &mut errors);
        errors.into_iter().map(|error| error.field).collect()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn one_widget(widget: Value) -> Value {
        json!({"widgets": [widget]})
    }

    #[test]
    fn the_valid_definition_has_no_errors() {
        assert!(paths_of(valid_definition()).is_empty());
    }

    #[test]
    fn widget_ids_are_checked_and_unique() {
        let bad = json!({"widgets": [heading("bad id!")]});
        assert_eq!(paths_of(bad), ["definition.widgets[0].id"]);
        let long = json!({"widgets": [heading(&"a".repeat(65))]});
        assert_eq!(paths_of(long), ["definition.widgets[0].id"]);
        let twice = json!({"widgets": [heading("same"), heading("same")]});
        assert_eq!(paths_of(twice), ["definition.widgets[1].id"]);
    }

    #[test]
    fn layout_bounds_are_checked() {
        let mut widget = heading("h");
        widget["layout"] = json!({"x": 11, "y": 0, "w": 2, "h": 1});
        assert_eq!(
            paths_of(one_widget(widget.clone())),
            ["definition.widgets[0].layout.w"]
        );
        widget["layout"] = json!({"x": -1, "y": 1001, "w": 0, "h": 41});
        assert_eq!(
            paths_of(one_widget(widget)),
            [
                "definition.widgets[0].layout.x",
                "definition.widgets[0].layout.y",
                "definition.widgets[0].layout.w",
                "definition.widgets[0].layout.h",
            ]
        );
    }

    #[test]
    fn headings_and_charts_have_their_parts() {
        let mut widget = heading("h");
        widget["chart"] = json!({"type": "line"});
        widget["query"] = query(json!([{"agg": "count"}]));
        assert_eq!(
            paths_of(one_widget(widget)),
            ["definition.widgets[0].chart", "definition.widgets[0].query"]
        );
        let bare = json!({
            "id": "c", "title": "Chart",
            "layout": {"x": 0, "y": 0, "w": 6, "h": 4}, "kind": "chart"
        });
        assert_eq!(
            paths_of(one_widget(bare)),
            ["definition.widgets[0].chart", "definition.widgets[0].query"]
        );
    }

    #[test]
    fn chart_settings_belong_to_their_type() {
        let count = query(json!([{"agg": "count"}]));
        let cases = [
            (json!({"type": "line", "stacked": true}), "chart.stacked"),
            (json!({"type": "line", "share": true}), "chart.share"),
            (json!({"type": "bar", "split": true}), "chart.split"),
            (json!({"type": "line", "fade_below": 5}), "chart.fade_below"),
            (json!({"type": "bar", "fade_below": 0}), "chart.fade_below"),
            (
                json!({"type": "bar", "fade_below": 1001}),
                "chart.fade_below",
            ),
            (json!({"type": "bar", "share": true}), "chart.share"),
        ];
        for (settings, path) in cases {
            let widget = chart(settings.clone(), count.clone());
            assert_eq!(
                paths_of(one_widget(widget)),
                [format!("definition.widgets[0].{path}")],
                "{settings}"
            );
        }
        let stacked_bar = chart(json!({"type": "bar", "stacked": true}), count);
        assert!(paths_of(one_widget(stacked_bar)).is_empty());
    }

    #[test]
    fn bar_h_needs_a_breakdown_and_number_is_one_measure() {
        let no_breakdown = chart(json!({"type": "bar_h"}), query(json!([{"agg": "count"}])));
        assert_eq!(
            paths_of(one_widget(no_breakdown)),
            ["definition.widgets[0].query.breakdown"]
        );
        let two = chart(
            json!({"type": "number"}),
            query(json!([{"agg": "count"}, {"agg": "count"}])),
        );
        assert_eq!(
            paths_of(one_widget(two)),
            ["definition.widgets[0].query.measures"]
        );
        let split = chart(
            json!({"type": "number"}),
            json!({
                "source": "traces",
                "measures": [{"agg": "count"}],
                "breakdown": {"field": "name"}
            }),
        );
        assert_eq!(
            paths_of(one_widget(split)),
            ["definition.widgets[0].query.breakdown"]
        );
    }

    #[test]
    fn query_shape_and_labels_are_checked_with_prefixed_paths() {
        let no_measures = chart(json!({"type": "line"}), query(json!([])));
        assert_eq!(
            paths_of(one_widget(no_measures)),
            ["definition.widgets[0].query.measures"]
        );
        let count_with_field = chart(
            json!({"type": "line"}),
            query(json!([{"agg": "count", "field": "duration_ns"}])),
        );
        assert_eq!(
            paths_of(one_widget(count_with_field)),
            ["definition.widgets[0].query.measures[0].field"]
        );
        let bad_limit = chart(
            json!({"type": "bar_h"}),
            json!({
                "source": "traces",
                "measures": [{"agg": "count"}],
                "breakdown": {"field": "name", "limit": 0}
            }),
        );
        assert_eq!(
            paths_of(one_widget(bad_limit)),
            ["definition.widgets[0].query.breakdown.limit"]
        );
        let long_label = chart(
            json!({"type": "line"}),
            query(json!([{"agg": "count", "label": "x".repeat(61)}])),
        );
        assert_eq!(
            paths_of(one_widget(long_label)),
            ["definition.widgets[0].query.measures[0].label"]
        );
    }

    #[test]
    fn titles_and_the_widget_count_are_checked() {
        let mut empty_heading = heading("h");
        empty_heading["title"] = json!("");
        assert_eq!(
            paths_of(one_widget(empty_heading)),
            ["definition.widgets[0].title"]
        );
        let mut long_chart = chart(json!({"type": "line"}), query(json!([{"agg": "count"}])));
        long_chart["title"] = json!("t".repeat(101));
        assert_eq!(
            paths_of(one_widget(long_chart)),
            ["definition.widgets[0].title"]
        );
        let fifty_one: Vec<Value> = (0..51).map(|i| heading(&format!("h{i}"))).collect();
        assert_eq!(
            paths_of(json!({"widgets": fifty_one})),
            ["definition.widgets"]
        );
    }

    #[test]
    fn an_oversized_definition_is_refused_at_its_path() {
        let filter = "x".repeat(MAX_DEFINITION_BYTES);
        let mut widget = chart(json!({"type": "line"}), query(json!([{"agg": "count"}])));
        widget["query"]["filter"] = json!(filter);
        assert_eq!(paths_of(one_widget(widget)), ["definition"]);
    }

    #[test]
    fn every_error_comes_back_at_once() {
        let mut bad_id = heading("bad id");
        bad_id["layout"] = json!({"x": 0, "y": 0, "w": 13, "h": 1});
        let widget = chart(json!({"type": "bar", "split": true}), query(json!([])));
        let mut bad_chart = widget;
        bad_chart["id"] = json!("bad id");
        assert_eq!(
            paths_of(json!({"widgets": [bad_id, bad_chart]})),
            [
                "definition.widgets[0].id",
                "definition.widgets[0].layout.w",
                "definition.widgets[1].id",
                "definition.widgets[1].chart.split",
                "definition.widgets[1].query.measures",
            ]
        );
    }

    #[test]
    fn names_and_descriptions_are_checked() {
        let mut errors = Vec::new();
        assert_eq!(
            checked_name("  Ada  ", "name", &mut errors),
            Some("Ada".to_owned())
        );
        assert!(checked_name("   ", "name", &mut errors).is_none());
        assert!(checked_name(&"n".repeat(101), "name", &mut errors).is_none());
        assert_eq!(
            errors.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(),
            ["name", "name"]
        );
        let mut errors = Vec::new();
        assert!(checked_description(Some(&"d".repeat(501)), "description", &mut errors).is_none());
        assert_eq!(errors[0].field, "description");
        assert!(checked_description(None, "description", &mut errors).is_none());
        assert!(errors.len() == 1);
    }

    #[test]
    fn a_huge_x_is_refused_without_overflow() {
        let mut widget = heading("h");
        widget["layout"] = json!({"x": i64::MAX, "y": 0, "w": 1, "h": 1});
        assert_eq!(
            paths_of(one_widget(widget)),
            [
                "definition.widgets[0].layout.x",
                "definition.widgets[0].layout.w"
            ]
        );
    }

    #[test]
    fn a_split_line_chart_needs_a_small_breakdown() {
        let split_line = |breakdown: Value| {
            let mut query = json!({"source": "traces", "measures": [{"agg": "count"}]});
            if !breakdown.is_null() {
                query["breakdown"] = breakdown;
            }
            chart(json!({"type": "line", "split": true}), query)
        };
        assert_eq!(
            paths_of(one_widget(split_line(Value::Null))),
            ["definition.widgets[0].query.breakdown"]
        );
        assert_eq!(
            paths_of(one_widget(split_line(
                json!({"field": "models", "limit": 7})
            ))),
            ["definition.widgets[0].query.breakdown.limit"]
        );
        assert!(
            paths_of(one_widget(split_line(
                json!({"field": "models", "limit": 6})
            )))
            .is_empty()
        );
        assert!(paths_of(one_widget(split_line(json!({"field": "models"})))).is_empty());
    }

    #[test]
    fn heading_titles_and_descriptions_are_trimmed() {
        let mut blank = heading("h");
        blank["title"] = json!("   ");
        assert_eq!(paths_of(one_widget(blank)), ["definition.widgets[0].title"]);
        let mut padded = heading("h");
        padded["title"] = json!(format!(" {} ", "t".repeat(100)));
        assert!(paths_of(one_widget(padded)).is_empty());

        let mut errors = Vec::new();
        assert_eq!(
            checked_description(Some("  Bot  "), "description", &mut errors),
            Some("Bot".to_owned())
        );
        assert_eq!(
            checked_description(Some("   "), "description", &mut errors),
            Some(String::new())
        );
        assert!(errors.is_empty());
    }

    // Route tests.

    fn base(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}/dashboards", seeded.project.id)
    }

    async fn send(
        app: &TestApp,
        method: Method,
        path: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> crate::test_support::TestResponse {
        let request = app.request(method, path).header("cookie", cookie);
        match body {
            Some(body) => request.json(&body).send().await,
            None => request.send().await,
        }
    }

    async fn create_named(app: &TestApp, seeded: &Seeded, name: &str) -> Dashboard {
        let response = send(
            app,
            Method::POST,
            &base(seeded),
            &seeded.cookie,
            Some(json!({"name": name, "definition": valid_definition()})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
        response.json()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_get_patch_delete_round_trip() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "  Support  ", "description": "Bot", "definition": valid_definition()})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let created: Dashboard = created.json();
        assert_eq!(created.name, "Support");
        assert_eq!(created.version, 1);
        assert_eq!(created.created_by, Some(seeded.user_id));
        assert_eq!(created.definition.widgets.len(), 4);

        let item = format!("{}/{}", base(&seeded), created.id);
        let fetched: Dashboard = send(&app, Method::GET, &item, &seeded.cookie, None)
            .await
            .json();
        assert_eq!(fetched, created);

        let listed = send(&app, Method::GET, &base(&seeded), &seeded.cookie, None).await;
        assert_eq!(listed.status, StatusCode::OK);
        let page: Value = listed.json();
        assert_eq!(page["data"].as_array().map(Vec::len), Some(1));
        assert!(page["data"][0].get("definition").is_none());

        app.state.clock.advance(Duration::from_secs(1));
        let patched = send(
            &app,
            Method::PATCH,
            &item,
            &seeded.cookie,
            Some(json!({"version": 1, "name": "Renamed"})),
        )
        .await;
        assert_eq!(patched.status, StatusCode::OK);
        let patched: Dashboard = patched.json();
        assert_eq!(patched.version, 2);
        assert_eq!(patched.name, "Renamed");
        assert_eq!(patched.description.as_deref(), Some("Bot"));
        assert_eq!(patched.definition, created.definition);
        assert_eq!(patched.updated_by, Some(seeded.user_id));
        assert_eq!(patched.created_at, created.created_at);
        assert!(patched.updated_at > created.updated_at);

        let deleted = send(&app, Method::DELETE, &item, &seeded.cookie, None).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = send(&app, Method::GET, &item, &seeded.cookie, None).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
        let missing = send(
            &app,
            Method::PATCH,
            &item,
            &seeded.cookie,
            Some(json!({"version": 2, "name": "x"})),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let again = send(&app, Method::DELETE, &item, &seeded.cookie, None).await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_version_is_409_with_the_stored_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_named(&app, &seeded, "Support").await;
        let item = format!("{}/{}", base(&seeded), created.id);
        let first = send(
            &app,
            Method::PATCH,
            &item,
            &seeded.cookie,
            Some(json!({"version": 1, "name": "A"})),
        )
        .await;
        assert_eq!(first.status, StatusCode::OK);

        let stale = send(
            &app,
            Method::PATCH,
            &item,
            &seeded.cookie,
            Some(json!({"version": 1, "name": "B"})),
        )
        .await;
        assert_eq!(stale.status, StatusCode::CONFLICT);
        let problem = stale.problem();
        assert_eq!(problem.code, ProblemCode::Conflict);
        assert_eq!(
            problem.detail.as_deref(),
            Some("the dashboard changed; current version is 2")
        );
        let unchanged: Dashboard = send(&app, Method::GET, &item, &seeded.cookie, None)
            .await
            .json();
        assert_eq!(unchanged.name, "A");
        assert_eq!(unchanged.version, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_list_pages_by_name_then_id() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let b = create_named(&app, &seeded, "b").await;
        create_named(&app, &seeded, "a").await;
        create_named(&app, &seeded, "c").await;

        let first = send(
            &app,
            Method::GET,
            &format!("{}?limit=2", base(&seeded)),
            &seeded.cookie,
            None,
        )
        .await;
        let first: Value = first.json();
        let names = |page: &Value| -> Vec<String> {
            page["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(names(&first), ["a", "b"]);
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        // The cursor is opaque: it is not the id.
        assert_ne!(cursor, b.id.to_string());

        let second: Value = send(
            &app,
            Method::GET,
            &format!("{}?limit=2&cursor={cursor}", base(&seeded)),
            &seeded.cookie,
            None,
        )
        .await
        .json();
        assert_eq!(names(&second), ["c"]);
        assert!(second["next_cursor"].is_null());

        // The cursor carries its position, so a dashboard deleted since still pages on.
        let item = format!("{}/{}", base(&seeded), b.id);
        let deleted = send(&app, Method::DELETE, &item, &seeded.cookie, None).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let after_delete: Value = send(
            &app,
            Method::GET,
            &format!("{}?cursor={cursor}", base(&seeded)),
            &seeded.cookie,
            None,
        )
        .await
        .json();
        assert_eq!(names(&after_delete), ["c"]);

        let garbage = send(
            &app,
            Method::GET,
            &format!("{}?cursor=nope", base(&seeded)),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
        assert_eq!(garbage.problem().errors[0].field, "cursor");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_cursors_are_400_on_cursor() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_named(&app, &seeded, "Support").await;
        let not_json = URL_SAFE_NO_PAD.encode(b"not json");
        let bad_uuid = URL_SAFE_NO_PAD.encode(br#"{"name":"x","id":"nope"}"#);
        let no_id = URL_SAFE_NO_PAD.encode(br#"{"name":"x"}"#);
        for cursor in ["!!!", not_json.as_str(), bad_uuid.as_str(), no_id.as_str()] {
            let response = send(
                &app,
                Method::GET,
                &format!("{}?cursor={cursor}", base(&seeded)),
                &seeded.cookie,
                None,
            )
            .await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{cursor}");
            let problem = response.problem();
            assert_eq!(problem.code, ProblemCode::BadRequest);
            assert_eq!(problem.errors[0].field, "cursor");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_and_delete_of_a_missing_id_are_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let item = format!("{}/{}", base(&seeded), Uuid::now_v7());
        let patched = send(
            &app,
            Method::PATCH,
            &item,
            &seeded.cookie,
            Some(json!({"version": 1, "name": "x"})),
        )
        .await;
        assert_eq!(patched.status, StatusCode::NOT_FOUND);
        let deleted = send(&app, Method::DELETE, &item, &seeded.cookie, None).await;
        assert_eq!(deleted.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_description_clears_and_an_omitted_one_is_kept() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Support", "description": "Bot", "definition": {"widgets": []}})),
        )
        .await;
        let created: Dashboard = created.json();
        assert_eq!(created.description.as_deref(), Some("Bot"));
        let item = format!("{}/{}", base(&seeded), created.id);

        let kept: Dashboard = send(
            &app,
            Method::PATCH,
            &item,
            &seeded.cookie,
            Some(json!({"version": 1, "name": "Renamed"})),
        )
        .await
        .json();
        assert_eq!(kept.description.as_deref(), Some("Bot"));

        let cleared = send(
            &app,
            Method::PATCH,
            &item,
            &seeded.cookie,
            Some(json!({"version": 2, "description": ""})),
        )
        .await;
        assert_eq!(cleared.status, StatusCode::OK);
        let cleared: Dashboard = cleared.json();
        assert_eq!(cleared.description, None);
        assert_eq!(cleared.name, "Renamed");

        let blank_on_create: Dashboard = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Blank", "description": "", "definition": {"widgets": []}})),
        )
        .await
        .json();
        assert_eq!(blank_on_create.description, None);

        // Blank after trimming is blank: stored as none on create and on PATCH.
        let spaces_on_create: Dashboard = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Spaces", "description": "   ", "definition": {"widgets": []}})),
        )
        .await
        .json();
        assert_eq!(spaces_on_create.description, None);
        let spaces_on_patch: Dashboard = send(
            &app,
            Method::PATCH,
            &format!("{}/{}", base(&seeded), created.id),
            &seeded.cookie,
            Some(json!({"version": 3, "description": " \t "})),
        )
        .await
        .json();
        assert_eq!(spaces_on_patch.description, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_definitions_answer_every_error_with_its_path() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut bad = heading("bad id");
        bad["layout"] = json!({"x": 0, "y": 0, "w": 13, "h": 1});
        let response = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "  ", "definition": {"widgets": [bad]}})),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let fields: Vec<String> = response
            .problem()
            .errors
            .into_iter()
            .map(|error| error.field)
            .collect();
        assert_eq!(
            fields,
            [
                "name",
                "definition.widgets[0].id",
                "definition.widgets[0].layout.w",
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_expression_names_its_path_and_position() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let filter = json!({"widgets": [
            heading("h"),
            chart(
                json!({"type": "line"}),
                json!({"source": "traces", "filter": "error_count >", "measures": [{"agg": "count"}]})
            ),
        ]});
        let response = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Broken", "definition": filter})),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let problem = response.problem();
        assert_eq!(problem.code, ProblemCode::InvalidFilter);
        assert_eq!(
            problem.errors[0].field,
            "definition.widgets[1].query.filter"
        );
        assert!(problem.errors[0].position.is_some());

        let condition = json!({"widgets": [chart(
            json!({"type": "number"}),
            json!({"source": "traces", "measures": [{"agg": "rate", "where": "error_count >"}]})
        )]});
        let response = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Broken", "definition": condition})),
        )
        .await;
        let problem = response.problem();
        assert_eq!(problem.code, ProblemCode::InvalidFilter);
        assert_eq!(
            problem.errors[0].field,
            "definition.widgets[0].query.measures[0].where"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_hundred_and_first_dashboard_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let user = seeded.user_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for index in 0..store::MAX_DASHBOARDS_PER_PROJECT {
                        store::insert(
                            conn,
                            NewDashboard {
                                project_id: project,
                                name: format!("d{index}"),
                                description: None,
                                definition: definition(json!({"widgets": []})),
                                user,
                            },
                            0,
                        )
                        .await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
        let refused = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "One more", "definition": {"widgets": []}})),
        )
        .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(
            refused.problem().detail.as_deref(),
            Some("too many dashboards")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn roles_and_keys_decide_who_writes() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let created = create_named(&app, &seeded, "Support").await;
        let body = json!({"name": "x", "definition": {"widgets": []}});

        let read = send(&app, Method::GET, &base(&seeded), &viewer.cookie, None).await;
        assert_eq!(read.status, StatusCode::OK);
        let refused = send(
            &app,
            Method::POST,
            &base(&seeded),
            &viewer.cookie,
            Some(body.clone()),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let item = format!("{}/{}", base(&seeded), created.id);
        let refused = send(
            &app,
            Method::PATCH,
            &item,
            &viewer.cookie,
            Some(json!({"version": 1, "name": "x"})),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let refused = send(&app, Method::DELETE, &item, &viewer.cookie, None).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);

        let allowed = send(
            &app,
            Method::POST,
            &base(&seeded),
            &member.cookie,
            Some(body.clone()),
        )
        .await;
        assert_eq!(allowed.status, StatusCode::CREATED);

        let read_key = app
            .request(Method::GET, &base(&seeded))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .send()
            .await;
        assert_eq!(read_key.status, StatusCode::OK);
        let key_write = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&body)
            .send()
            .await;
        assert_eq!(key_write.status, StatusCode::FORBIDDEN);
        assert_eq!(key_write.header("x-test-access"), Some("denied-403"));

        let read_pat = app.personal_token(member.id, false).await;
        let read = app
            .request(Method::GET, &base(&seeded))
            .header("authorization", &format!("Bearer {read_pat}"))
            .send()
            .await;
        assert_eq!(read.status, StatusCode::OK);
        let pat_write = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {read_pat}"))
            .json(&body)
            .send()
            .await;
        assert_eq!(pat_write.status, StatusCode::FORBIDDEN);

        let write_pat = app.personal_token(member.id, true).await;
        let written = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {write_pat}"))
            .json(&body)
            .send()
            .await;
        assert_eq!(written.status, StatusCode::CREATED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_projects_dashboard_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _key) = app.add_project(&seeded, "other", None).await;
        let created = create_named(&app, &seeded, "Support").await;
        let path = format!("/api/v1/projects/{}/dashboards/{}", other.id, created.id);
        let response = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    async fn dashboard_count(app: &TestApp, project: ProjectId) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM dashboards WHERE project_id = ?")
            .bind(project.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_project_deletes_its_dashboards() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _key) = app.add_project(&seeded, "other", None).await;
        let project = other.id;
        let user = seeded.user_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert(
                        conn,
                        NewDashboard {
                            project_id: project,
                            name: "kept until the project goes".to_owned(),
                            description: None,
                            definition: definition(json!({"widgets": []})),
                            user,
                        },
                        0,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(dashboard_count(&app, project).await, 1);
        app.state
            .meta
            .write(move |conn| Box::pin(async move { delete_project(conn, project).await }))
            .await
            .unwrap();
        assert_eq!(dashboard_count(&app, project).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stored_dashboard_is_read_back_unchanged() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_named(&app, &seeded, "Support").await;
        let project = seeded.project.id;
        let row = store::get(app.state.meta.reader(), project, created.id)
            .await
            .unwrap()
            .expect("the dashboard is stored");
        assert_eq!(row.definition, created.definition);
        assert_eq!(row.info.version, 1);
    }

    // Spec 05 § Dashboard queries. The example dataset has five traces in the window below.
    const FROM: &str = "2026-01-01T10:00:00Z";
    const TO: &str = "2026-01-01T11:00:00Z";
    const TEN_MINUTES: i64 = 600_000_000_000;
    const DAY: i64 = 86_400_000_000_000;

    /// A series over the example window: count and error rate, split by `name`.
    fn series_body() -> Value {
        json!({
            "from": FROM, "to": TO, "bucket": "10m", "shape": "series",
            "query": {
                "source": "traces",
                "measures": [
                    {"agg": "count"},
                    {"agg": "rate", "where": "error_count > 0"}
                ],
                "breakdown": {"field": "name", "limit": 3}
            }
        })
    }

    /// A total over the example window with the previous range.
    fn total_body() -> Value {
        json!({
            "from": FROM, "to": TO, "shape": "total", "compare": true,
            "query": {"source": "traces", "measures": [{"agg": "count"}]}
        })
    }

    /// A loaded example project.
    async fn loaded() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    fn query_path(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/dashboards/query")
    }

    /// POSTs `body` to the widget query of `seeded`'s project as `cookie`.
    async fn post_query(app: &TestApp, seeded: &Seeded, body: &Value) -> (StatusCode, Value) {
        let response = send(
            app,
            Method::POST,
            &query_path(seeded.project.id),
            &seeded.cookie,
            Some(body.clone()),
        )
        .await;
        (response.status, response.json())
    }

    /// Asserts a `400` with `code` and an error on `field`.
    fn assert_refused_at(status: StatusCode, body: &Value, code: &str, field: &str) {
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], code, "{body}");
        let errors = body["errors"].as_array().expect("field errors");
        assert!(
            errors.iter().any(|error| error["field"] == field),
            "{field} in {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_series_has_points_and_rows() {
        let (app, seeded) = loaded().await;
        let (status, body) = post_query(&app, &seeded, &series_body()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["shape"], "series");
        assert_eq!(body["bucket_ns"], TEN_MINUTES);
        let series = body["series"].as_array().expect("series");
        assert!(!series.is_empty(), "{body}");
        for item in series {
            assert!(item["measure"].is_u64(), "{item}");
            assert!(item["other"].is_boolean(), "{item}");
            let points = item["points"].as_array().expect("points");
            for point in points {
                assert!(point["time"].is_string(), "{point}");
                assert!(point["rows"].is_u64(), "{point}");
            }
        }
        assert!(
            series.iter().any(|item| item["group"].is_string()),
            "the breakdown names its groups: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_total_has_values_and_previous() {
        let (app, seeded) = loaded().await;
        let (status, body) = post_query(&app, &seeded, &total_body()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["shape"], "total");
        let rows = body["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 1, "{body}");
        assert_eq!(rows[0]["values"], json!([5.0]), "{body}");
        assert_eq!(rows[0]["other"], false);
        assert!(rows[0]["previous"].is_array(), "{body}");
        assert!(rows[0].get("group").is_none(), "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_without_compare_has_no_previous() {
        let (app, seeded) = loaded().await;
        let mut body = total_body();
        body["compare"] = json!(false);
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert!(answer["rows"][0].get("previous").is_none(), "{answer}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_refuses_bad_fields_at_their_paths() {
        let (app, seeded) = loaded().await;

        let mut body = series_body();
        body["query"]["measures"] = json!([{"agg": "count"}, {"agg": "sum", "field": "name"}]);
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_refused_at(status, &answer, "bad_request", "query.measures[1].field");

        let mut body = series_body();
        body["query"]["measures"] = json!([{"agg": "rate", "where": "error_count >"}]);
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_refused_at(status, &answer, "invalid_filter", "query.measures[0].where");
        assert!(
            answer["errors"][0]["position"].is_object(),
            "a DSL error has a position: {answer}"
        );

        let mut body = series_body();
        body["query"]["filter"] = json!("nope =");
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_refused_at(status, &answer, "invalid_filter", "query.filter");

        let mut body = series_body();
        body["filter"] = json!("nope =");
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_refused_at(status, &answer, "invalid_filter", "filter");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_refuses_a_bad_range_and_a_fine_bucket() {
        let (app, seeded) = loaded().await;
        let mut body = total_body();
        body["from"] = json!("not a time");
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_refused_at(status, &answer, "bad_request", "from");

        let mut body = total_body();
        body["from"] = json!(TO);
        body["to"] = json!(FROM);
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_refused_at(status, &answer, "bad_request", "from");

        // Two days of one-minute buckets is 2,880 buckets, past the 2,000 limit.
        let mut body = series_body();
        body["from"] = json!("2026-01-01T00:00:00Z");
        body["to"] = json!("2026-01-03T00:00:00Z");
        body["bucket"] = json!("1m");
        let (status, answer) = post_query(&app, &seeded, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert_eq!(answer["code"], "bad_request", "{answer}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_is_open_to_read_credentials_only() {
        let (app, seeded) = loaded().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let token = app.personal_token(viewer.id, false).await;
        let path = query_path(seeded.project.id);
        let body = total_body();

        let response = send(
            &app,
            Method::POST,
            &path,
            &viewer.cookie,
            Some(body.clone()),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "viewer session");

        let response = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {token}"))
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "read-only PAT");

        let response = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "read key");

        let response = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {}", seeded.ingest_key))
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "ingest key");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_refuses_another_projects_key_like_other_reads() {
        let (app, seeded) = loaded().await;
        let (other, _key) = app.add_project(&seeded, "other", None).await;
        let response = app
            .request(Method::POST, &query_path(other.id))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&total_body())
            .send()
            .await;
        let read = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/metrics?from={FROM}&to={TO}", other.id),
            )
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .send()
            .await;
        assert_eq!(response.status, read.status);
        assert_ne!(response.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_answers_are_cached_for_ten_seconds() {
        let (app, seeded) = loaded().await;
        let (_, first) = post_query(&app, &seeded, &total_body()).await;
        assert_eq!(first["rows"][0]["values"], json!([5.0]));

        let trace = TraceId::from_str("a1000000000000000000000000000009").unwrap();
        app.commit_trace_at(
            &seeded.project,
            trace,
            9,
            1_767_261_600_000_000_000 + 40 * 60_000_000_000,
        )
        .await;
        let (_, cached) = post_query(&app, &seeded, &total_body()).await;
        assert_eq!(cached["rows"][0]["values"], json!([5.0]), "still cached");

        app.state.clock.advance(Duration::from_secs(11));
        let (_, fresh) = post_query(&app, &seeded, &total_body()).await;
        assert_eq!(fresh["rows"][0]["values"], json!([6.0]), "expired");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clearing_a_project_drops_its_widget_answers() {
        let (app, seeded) = loaded().await;
        let (_, first) = post_query(&app, &seeded, &total_body()).await;
        assert_eq!(first["rows"][0]["values"], json!([5.0]));

        let trace = TraceId::from_str("a1000000000000000000000000000008").unwrap();
        app.commit_trace_at(
            &seeded.project,
            trace,
            8,
            1_767_261_600_000_000_000 + 40 * 60_000_000_000,
        )
        .await;
        app.state.clear_answers(seeded.project.id);
        let (_, fresh) = post_query(&app, &seeded, &total_body()).await;
        assert_eq!(fresh["rows"][0]["values"], json!([6.0]));
    }

    /// D-P-21: the widget answer is keyed by the org's window, so a limit set within the TTL is
    /// applied at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_query_is_keyed_by_the_data_window() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let old = TraceId::from_str("a9000000000000000000000000000001").unwrap();
        let fresh = TraceId::from_str("a9000000000000000000000000000002").unwrap();
        app.commit_trace_at(&seeded.project, old, 1, now - 40 * DAY)
            .await;
        app.commit_trace_at(&seeded.project, fresh, 2, now - DAY)
            .await;
        let mut body = total_body();
        body["from"] = json!("2025-11-01T00:00:00Z");
        body["to"] = json!("2026-01-02T00:00:00Z");

        let (status, first) = post_query(&app, &seeded, &body).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["rows"][0]["values"], json!([2.0]));

        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"data_access_days": 30}),
        );
        let (_, limited) = post_query(&app, &seeded, &body).await;
        assert_eq!(limited["rows"][0]["values"], json!([1.0]));
    }
}
