//! The prompt tools of the MCP server (track V M8, D-V-39): `list_prompts` and `get_prompt`.
//! Both read the registry of `meta::prompts` and answer what the prompt routes answer.

use rmcp::model::JsonObject;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::output;
use super::record_tool;
use super::tools::arguments;
use crate::audit;
use crate::auth::principal::AuthUser;
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::user_project;
use crate::http::error::ApiError;
use crate::meta::prompts::{self as store, PromptFilter, PromptRow};
use crate::prompts::{DEFAULT_LABEL, LATEST};
use crate::routes::prompts::version_json;

pub(crate) const LIST_PROMPTS: &str = "list_prompts";
pub(crate) const GET_PROMPT: &str = "get_prompt";

/// `list_prompts`' default and highest `limit`.
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    project: String,
    q: Option<String>,
    label: Option<String>,
    limit: Option<i64>,
    max_chars: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArguments {
    project: String,
    name: String,
    label: Option<String>,
    version: Option<i64>,
}

/// `{"prompts": [{name, type, description?, tags, latest_version, labels}], "truncated", "notes"}`:
/// the project's prompts ordered by name, the first `limit` (default 50, 1 to 200). `q` is a
/// case-insensitive part of the name; `label` keeps the prompts carrying it (`latest` keeps all).
/// `labels` maps each stored label to its version; `latest` is not listed. Fitted to `max_chars`
/// on `prompts`.
pub(crate) async fn list_prompts(
    state: &AppState,
    user: &AuthUser,
    audit: Option<&audit::Ctx>,
    raw: JsonObject,
) -> Result<Value, ApiError> {
    let args: ListArguments = arguments(raw)?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    let limit = usize::try_from(limit)
        .ok()
        .filter(|limit| (1..=MAX_LIMIT).contains(limit))
        .ok_or_else(|| ApiError::BadRequest {
            detail: Some("limit must be 1 to 200".to_owned()),
            errors: Vec::new(),
        })?;
    let max_chars = output::max_chars(args.max_chars)?;
    let access = user_project(state, user, &args.project, ProjectPerm::View).await?;
    record_tool(state, audit, LIST_PROMPTS, &access, None).await?;
    let filter = PromptFilter {
        q: args.q,
        label: args.label.filter(|label| label != LATEST),
        tag: None,
    };
    let rows = store::list_prompts(state.meta.reader(), access.project.id, &filter).await?;
    let more = rows.len() > limit;
    let prompts: Vec<Value> = rows.iter().take(limit).map(prompt_item).collect();
    let mut notes = Vec::new();
    if more {
        notes.push(json!("more prompts than limit; narrow with q or label"));
    }
    let mut body = Map::new();
    body.insert("prompts".to_owned(), Value::Array(prompts));
    body.insert("truncated".to_owned(), Value::Bool(more));
    body.insert("notes".to_owned(), Value::Array(notes));
    output::fit_list(
        &mut body,
        "prompts",
        max_chars,
        &format!("result cut at {max_chars} characters; later prompts left out"),
    );
    Ok(Value::Object(body))
}

/// One prompt of `list_prompts`. `description` is left out when the prompt has none.
fn prompt_item(row: &PromptRow) -> Value {
    let mut item = Map::new();
    item.insert("name".to_owned(), json!(row.name));
    item.insert("type".to_owned(), json!(row.prompt_type.as_str()));
    if let Some(description) = &row.description {
        item.insert("description".to_owned(), json!(description));
    }
    item.insert("tags".to_owned(), json!(row.tags));
    item.insert("latest_version".to_owned(), json!(row.latest_version));
    let labels: Map<String, Value> = row
        .labels
        .iter()
        .map(|label| (label.label.clone(), json!(label.version)))
        .collect();
    item.insert("labels".to_owned(), Value::Object(labels));
    Value::Object(item)
}

/// The version `GET …/prompts/{name}/resolve` answers for the same arguments: `version` when
/// given, else the label (`latest` is the latest version, the default is `production`). Not
/// fitted to `max_chars`: a version is returned whole, as `get_schema` returns its catalog.
pub(crate) async fn get_prompt(
    state: &AppState,
    user: &AuthUser,
    audit: Option<&audit::Ctx>,
    raw: JsonObject,
) -> Result<Value, ApiError> {
    let args: GetArguments = arguments(raw)?;
    if args.label.is_some() && args.version.is_some() {
        return Err(ApiError::BadRequest {
            detail: Some("give label or version, not both".to_owned()),
            errors: Vec::new(),
        });
    }
    let access = user_project(state, user, &args.project, ProjectPerm::View).await?;
    record_tool(state, audit, GET_PROMPT, &access, None).await?;
    let reader = state.meta.reader();
    let row = store::get_prompt(reader, access.project.id, &args.name)
        .await?
        .ok_or(ApiError::NotFound)?;
    let found = match (args.version, args.label.as_deref()) {
        (Some(version), _) => store::get_version(reader, row.id, version).await?,
        (None, Some(LATEST)) => store::get_version(reader, row.id, row.latest_version).await?,
        (None, label) => {
            store::resolve_label(reader, row.id, label.unwrap_or(DEFAULT_LABEL)).await?
        }
    };
    let version = found.ok_or(ApiError::NotFound)?;
    serde_json::to_value(version_json(&row, &version)?)
        .map_err(|error| ApiError::Internal(Box::new(error)))
}
