//! `just api-examples`: `fixtures/api` example responses, produced by the real read handlers over `fixtures/api/dataset` (D-G-17, D-G-25).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::http::{Method, StatusCode};
use cetra_core::ids::ProjectId;
use cetra_core::score::Score;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::assistant::{
    AssistantConversation, AssistantConversationDetail, AssistantSettings, AssistantStatus,
};
use crate::api::common::Page;
use crate::api::dashboards::WidgetQueryResponse;
use crate::api::datasets::{Dataset, DatasetItem, DatasetVersion, ExperimentTrends, ItemChange};
use crate::api::facets::FacetsResponse;
use crate::api::prompts::{Prompt, PromptLabelEvent, PromptMetrics, PromptSettings, PromptVersion};
use crate::api::schema::SchemaResponse;
use crate::api::sessions::{SessionDetail, SessionSummary};
use crate::api::sql::SqlQueryResult;
use crate::api::traces::{SpanDetail, SpanSummary, TraceDetail, TraceSummary};
use crate::api::users::{EndUserDetail, EndUserSummary};
use crate::test_support::{EXAMPLE_CONVERSATION, EXAMPLE_PROJECT, EXAMPLE_PROMPT, TestApp};

const MEDIA: &str = "497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581";
const EXAMPLE_DATASET: &str = "01900000-0000-7000-8000-000000000c01";
/// Item `…0d02` of `faq`: edited once, so its current revision is `…0d14`.
const EXAMPLE_ITEM: &str = "01900000-0000-7000-8000-000000000d02";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Example {
    operation_id: String,
    path_params: BTreeMap<String, String>,
    query: BTreeMap<String, String>,
    status: u16,
    content_type: String,
    file: String,
}

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn example_created(
    operation_id: &str,
    path_params: BTreeMap<String, String>,
    file: &str,
) -> Example {
    Example {
        operation_id: operation_id.to_owned(),
        path_params,
        query: BTreeMap::new(),
        status: 201,
        content_type: "application/json".to_owned(),
        file: file.to_owned(),
    }
}

fn example_accepted(
    operation_id: &str,
    path_params: BTreeMap<String, String>,
    file: &str,
) -> Example {
    Example {
        operation_id: operation_id.to_owned(),
        path_params,
        query: BTreeMap::new(),
        status: 202,
        content_type: "application/json".to_owned(),
        file: file.to_owned(),
    }
}

fn example_status(operation_id: &str, file: &str, status: u16) -> Example {
    Example {
        operation_id: operation_id.to_owned(),
        path_params: BTreeMap::new(),
        query: BTreeMap::new(),
        status,
        content_type: "application/json".to_owned(),
        file: file.to_owned(),
    }
}

fn example(
    operation_id: &str,
    path_params: BTreeMap<String, String>,
    query: BTreeMap<String, String>,
    file: &str,
    content_type: &str,
) -> Example {
    Example {
        operation_id: operation_id.to_owned(),
        path_params,
        query,
        status: 200,
        content_type: content_type.to_owned(),
        file: file.to_owned(),
    }
}

/// Canonical text of a value: pretty JSON, one trailing newline.
///
/// Copied from `tests/support/mod.rs`, which the integration tests use.
fn canonical_json<T: Serialize + DeserializeOwned>(value: &T) -> String {
    // One parse-and-print pass. serde_json's shortest float form is not always a fixed
    // point of its parser, and the example files must round-trip through the typed value.
    let once = serde_json::to_string_pretty(value).unwrap_or_else(|error| panic!("{error}"));
    let parsed: T = serde_json::from_str(&once).unwrap_or_else(|error| panic!("{error}"));
    let twice = serde_json::to_string_pretty(&parsed).unwrap_or_else(|error| panic!("{error}"));
    format!("{twice}\n")
}

fn api_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/api")
}

fn read_json<T: DeserializeOwned>(path: &Path) -> T {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The generated files and the manifest entries, in one place.
#[derive(Default)]
struct Output {
    files: BTreeMap<String, String>,
    manifest: Vec<Example>,
}

impl Output {
    /// A `200` JSON response with no path or query parameters beyond `path_params` and `query`.
    fn add<T: Serialize + DeserializeOwned>(
        &mut self,
        operation_id: &str,
        path_params: &[(&str, &str)],
        query: &[(&str, &str)],
        file: &str,
        value: &T,
    ) {
        self.files.insert(file.to_owned(), canonical_json(value));
        self.manifest.push(example(
            operation_id,
            params(path_params),
            params(query),
            file,
            "application/json",
        ));
    }
}

/// `GET` of the example project's `path_and_query`. Asserts `200` and parses the body as `T`.
async fn fetch<T: Serialize + DeserializeOwned>(
    app: &TestApp,
    cookie: &str,
    path_and_query: &str,
) -> T {
    let path = format!("/api/v1/projects/{EXAMPLE_PROJECT}{path_and_query}");
    let response = app
        .request(Method::GET, &path)
        .header("cookie", cookie)
        .send()
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{path}: {}",
        String::from_utf8_lossy(&response.body)
    );
    response.json()
}

/// `GET` of `path`, an absolute path. Asserts `200` and parses the body as `T`.
async fn get_at<T: DeserializeOwned>(app: &TestApp, cookie: &str, path: &str) -> T {
    let response = app
        .request(Method::GET, path)
        .header("cookie", cookie)
        .send()
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{path}: {}",
        String::from_utf8_lossy(&response.body)
    );
    response.json()
}

/// `POST` of `body` to the example project's `path`. Asserts `200` and parses the body as `T`.
async fn post_json<T: Serialize + DeserializeOwned>(
    app: &TestApp,
    cookie: &str,
    path: &str,
    body: &Value,
) -> T {
    let path = format!("/api/v1/projects/{EXAMPLE_PROJECT}{path}");
    let response = app
        .request(Method::POST, &path)
        .header("cookie", cookie)
        .json(body)
        .send()
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{path}: {}",
        String::from_utf8_lossy(&response.body)
    );
    response.json()
}

async fn generate() -> (BTreeMap<String, String>, Vec<Example>) {
    let app = TestApp::new().await;
    let cookie = app.seed_examples().await;
    let project: ProjectId = EXAMPLE_PROJECT
        .parse()
        .unwrap_or_else(|error| panic!("{error}"));
    app.load_example_dataset(project).await;
    app.seed_example_faq_dataset().await;
    app.seed_example_conversation().await;
    app.seed_example_assistant_settings().await;
    app.seed_example_prompts().await;
    let cookie = cookie.as_str();
    let mut out = Output::default();

    let traces: Page<TraceSummary> = fetch(&app, cookie, "/traces").await;
    out.add("list_traces", &[], &[], "responses/traces.json", &traces);

    let traces_eval: Page<TraceSummary> = fetch(&app, cookie, "/traces?include_eval=true").await;
    out.add(
        "list_traces",
        &[],
        &[("include_eval", "true")],
        "responses/traces-include-eval.json",
        &traces_eval,
    );

    for trace in &traces_eval.data {
        let trace_id = trace.trace_id.to_string();
        let detail: TraceDetail = fetch(&app, cookie, &format!("/traces/{trace_id}")).await;
        out.add(
            "get_trace",
            &[("trace_id", &trace_id)],
            &[],
            &format!("responses/traces/{trace_id}.json"),
            &detail,
        );
        for node in &detail.spans {
            let span_id = node.span.span_id.to_string();
            let span: SpanDetail =
                fetch(&app, cookie, &format!("/traces/{trace_id}/spans/{span_id}")).await;
            out.add(
                "get_span",
                &[("trace_id", &trace_id), ("span_id", &span_id)],
                &[],
                &format!("responses/spans/{trace_id}/{span_id}.json"),
                &span,
            );
        }
    }

    let spans: Page<SpanSummary> = fetch(&app, cookie, "/spans").await;
    out.add("list_spans", &[], &[], "responses/spans.json", &spans);

    let sessions: Page<SessionSummary> = fetch(&app, cookie, "/sessions").await;
    out.add(
        "list_sessions",
        &[],
        &[],
        "responses/sessions.json",
        &sessions,
    );
    for session in &sessions.data {
        let session_id = session.session_id.as_str();
        let detail: SessionDetail = fetch(&app, cookie, &format!("/sessions/{session_id}")).await;
        out.add(
            "get_session",
            &[("session_id", session_id)],
            &[],
            &format!("responses/sessions/{session_id}.json"),
            &detail,
        );
    }

    let sessions_eval: Page<SessionSummary> =
        fetch(&app, cookie, "/sessions?include_eval=true").await;
    out.add(
        "list_sessions",
        &[],
        &[("include_eval", "true")],
        "responses/sessions-include-eval.json",
        &sessions_eval,
    );
    let session_eval: SessionDetail =
        fetch(&app, cookie, "/sessions/sess-001?include_eval=true").await;
    out.add(
        "get_session",
        &[("session_id", "sess-001")],
        &[("include_eval", "true")],
        "responses/sessions/sess-001-include-eval.json",
        &session_eval,
    );

    let users: Page<EndUserSummary> = fetch(&app, cookie, "/users").await;
    out.add("list_users", &[], &[], "responses/users.json", &users);
    for user in &users.data {
        let user_id = user.user_id.as_str();
        let detail: EndUserDetail = fetch(&app, cookie, &format!("/users/{user_id}")).await;
        out.add(
            "get_user",
            &[("user_id", user_id)],
            &[],
            &format!("responses/users/{user_id}.json"),
            &detail,
        );
    }

    let users_eval: Page<EndUserSummary> = fetch(&app, cookie, "/users?include_eval=true").await;
    out.add(
        "list_users",
        &[],
        &[("include_eval", "true")],
        "responses/users-include-eval.json",
        &users_eval,
    );
    let user_eval: EndUserDetail = fetch(&app, cookie, "/users/user-42?include_eval=true").await;
    out.add(
        "get_user",
        &[("user_id", "user-42")],
        &[("include_eval", "true")],
        "responses/users/user-42-include-eval.json",
        &user_eval,
    );

    let scores: Page<Score> = fetch(&app, cookie, "/scores").await;
    out.add("list_scores", &[], &[], "responses/scores.json", &scores);

    let conversations: Page<AssistantConversation> =
        fetch(&app, cookie, "/assistant/conversations").await;
    out.add(
        "list_assistant_conversations",
        &[],
        &[],
        "responses/assistant-conversations.json",
        &conversations,
    );
    let conversation: AssistantConversationDetail = fetch(
        &app,
        cookie,
        &format!("/assistant/conversations/{EXAMPLE_CONVERSATION}"),
    )
    .await;
    out.add(
        "get_assistant_conversation",
        &[("id", EXAMPLE_CONVERSATION)],
        &[],
        &format!("responses/assistant-conversations/{EXAMPLE_CONVERSATION}.json"),
        &conversation,
    );
    let org_setting: AssistantSettings = get_at(&app, cookie, "/api/v1/orgs/acme/assistant").await;
    out.add(
        "get_org_assistant",
        &[("org", "acme")],
        &[],
        "responses/org-assistant.json",
        &org_setting,
    );
    let assistant_status: AssistantStatus = fetch(&app, cookie, "/assistant").await;
    out.add(
        "get_project_assistant",
        &[],
        &[],
        "responses/project-assistant.json",
        &assistant_status,
    );

    let datasets: Page<Dataset> = fetch(&app, cookie, "/datasets").await;
    out.add(
        "list_datasets",
        &[],
        &[],
        "responses/datasets.json",
        &datasets,
    );
    let dataset: Dataset = fetch(&app, cookie, &format!("/datasets/{EXAMPLE_DATASET}")).await;
    out.add(
        "get_dataset",
        &[("dataset", EXAMPLE_DATASET)],
        &[],
        &format!("responses/datasets/{EXAMPLE_DATASET}.json"),
        &dataset,
    );
    let versions: Page<DatasetVersion> = fetch(
        &app,
        cookie,
        &format!("/datasets/{EXAMPLE_DATASET}/versions"),
    )
    .await;
    out.add(
        "list_dataset_versions",
        &[("dataset", EXAMPLE_DATASET)],
        &[],
        &format!("responses/datasets/{EXAMPLE_DATASET}/versions.json"),
        &versions,
    );
    let items: Page<DatasetItem> =
        fetch(&app, cookie, &format!("/datasets/{EXAMPLE_DATASET}/items")).await;
    out.add(
        "list_dataset_items",
        &[("dataset", EXAMPLE_DATASET)],
        &[],
        &format!("responses/datasets/{EXAMPLE_DATASET}/items.json"),
        &items,
    );
    let items_v1: Page<DatasetItem> = fetch(
        &app,
        cookie,
        &format!("/datasets/{EXAMPLE_DATASET}/items?version=1"),
    )
    .await;
    out.add(
        "list_dataset_items",
        &[("dataset", EXAMPLE_DATASET)],
        &[("version", "1")],
        &format!("responses/datasets/{EXAMPLE_DATASET}/items-v1.json"),
        &items_v1,
    );
    let item: DatasetItem = fetch(
        &app,
        cookie,
        &format!("/datasets/{EXAMPLE_DATASET}/items/{EXAMPLE_ITEM}"),
    )
    .await;
    out.add(
        "get_dataset_item",
        &[("dataset", EXAMPLE_DATASET), ("item_id", EXAMPLE_ITEM)],
        &[],
        &format!("responses/datasets/{EXAMPLE_DATASET}/items/{EXAMPLE_ITEM}.json"),
        &item,
    );
    // Version 2 edited `EXAMPLE_ITEM`: one changed item, its revision before and after.
    let changes: Page<ItemChange> = fetch(
        &app,
        cookie,
        &format!("/datasets/{EXAMPLE_DATASET}/versions/2/changes"),
    )
    .await;
    out.add(
        "list_dataset_version_changes",
        &[("dataset", EXAMPLE_DATASET), ("version", "2")],
        &[],
        &format!("responses/datasets/{EXAMPLE_DATASET}/versions/2/changes.json"),
        &changes,
    );
    let trends: ExperimentTrends = fetch(
        &app,
        cookie,
        &format!("/dataset-trends?ids={EXAMPLE_DATASET}"),
    )
    .await;
    out.add(
        "list_experiment_trends",
        &[],
        &[("ids", EXAMPLE_DATASET)],
        "responses/dataset-trends.json",
        &trends,
    );

    // The prompt registry: the example prompts of `fixtures/api/README.md` § Fixed identifiers.
    let prompts: Page<Prompt> = fetch(&app, cookie, "/prompts").await;
    out.add("list_prompts", &[], &[], "responses/prompts.json", &prompts);
    let prompt: Prompt = fetch(&app, cookie, &format!("/prompts/{EXAMPLE_PROMPT}")).await;
    out.add(
        "get_prompt",
        &[("prompt", EXAMPLE_PROMPT)],
        &[],
        &format!("responses/prompts/{EXAMPLE_PROMPT}.json"),
        &prompt,
    );
    let production: PromptVersion =
        fetch(&app, cookie, &format!("/prompts/{EXAMPLE_PROMPT}/resolve")).await;
    out.add(
        "get_prompt_resolve",
        &[("prompt", EXAMPLE_PROMPT)],
        &[],
        &format!("responses/prompts/{EXAMPLE_PROMPT}/resolve.json"),
        &production,
    );
    let staging: PromptVersion = fetch(
        &app,
        cookie,
        &format!("/prompts/{EXAMPLE_PROMPT}/resolve?label=staging"),
    )
    .await;
    out.add(
        "get_prompt_resolve",
        &[("prompt", EXAMPLE_PROMPT)],
        &[("label", "staging")],
        &format!("responses/prompts/{EXAMPLE_PROMPT}/resolve-staging.json"),
        &staging,
    );
    let versions: Page<PromptVersion> =
        fetch(&app, cookie, &format!("/prompts/{EXAMPLE_PROMPT}/versions")).await;
    out.add(
        "list_prompt_versions",
        &[("prompt", EXAMPLE_PROMPT)],
        &[],
        &format!("responses/prompts/{EXAMPLE_PROMPT}/versions.json"),
        &versions,
    );
    for number in ["1", "2"] {
        let version: PromptVersion = fetch(
            &app,
            cookie,
            &format!("/prompts/{EXAMPLE_PROMPT}/versions/{number}"),
        )
        .await;
        out.add(
            "get_prompt_version",
            &[("prompt", EXAMPLE_PROMPT), ("version", number)],
            &[],
            &format!("responses/prompts/{EXAMPLE_PROMPT}/versions/{number}.json"),
            &version,
        );
    }
    let events: Page<PromptLabelEvent> = fetch(
        &app,
        cookie,
        &format!("/prompts/{EXAMPLE_PROMPT}/label-events"),
    )
    .await;
    out.add(
        "list_prompt_label_events",
        &[("prompt", EXAMPLE_PROMPT)],
        &[],
        &format!("responses/prompts/{EXAMPLE_PROMPT}/label-events.json"),
        &events,
    );
    let metrics: PromptMetrics = fetch(
        &app,
        cookie,
        &format!(
            "/prompts/{EXAMPLE_PROMPT}/metrics?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z"
        ),
    )
    .await;
    out.add(
        "get_prompt_metrics",
        &[("prompt", EXAMPLE_PROMPT)],
        &[
            ("from", "2026-01-01T00:00:00Z"),
            ("to", "2026-01-02T00:00:00Z"),
        ],
        &format!("responses/prompts/{EXAMPLE_PROMPT}/metrics.json"),
        &metrics,
    );
    let settings: PromptSettings = fetch(&app, cookie, "/prompt-settings").await;
    out.add(
        "get_prompt_settings",
        &[],
        &[],
        "responses/prompt-settings.json",
        &settings,
    );

    // Every facet the web shows open by default, and every preset field, has an answer here.
    const FACET_WINDOW: &str = "from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z";
    const FACETS: &[(&str, &str)] = &[
        ("traces", "status"),
        ("traces", "name"),
        ("traces", "environment"),
        ("traces", "tags"),
        ("traces", "models"),
        ("traces", "metadata"),
        ("traces", "duration_ms"),
        ("traces", "cost_total"),
        ("spans", "span_kind"),
        ("spans", "status"),
        ("spans", "name"),
        ("spans", "model"),
        ("spans", "duration_ms"),
        ("spans", "cost_total"),
        ("sessions", "user_id"),
        ("sessions", "tags"),
        ("sessions", "duration_ms"),
        ("sessions", "cost_total"),
    ];
    for (view, field) in FACETS {
        let facet: FacetsResponse = fetch(
            &app,
            cookie,
            &format!("/facets?view={view}&field={field}&{FACET_WINDOW}"),
        )
        .await;
        out.add(
            "get_facets",
            &[],
            &[("view", view), ("field", field)],
            &format!("responses/facets-{view}-{field}.json"),
            &facet,
        );
    }

    let request: Value = read_json(&api_dir().join("requests/sql-query.json"));
    let sql: SqlQueryResult = post_json(&app, cookie, "/sql", &request).await;
    out.add("run_sql", &[], &[], "static/sql-result.json", &sql);

    // Widget queries (spec 05 § Dashboard queries): a series and a total with `compare`.
    let series_request: Value = read_json(&api_dir().join("requests/dashboard-query-series.json"));
    let series: WidgetQueryResponse =
        post_json(&app, cookie, "/dashboards/query", &series_request).await;
    out.add(
        "query_widget",
        &[],
        &[],
        "responses/dashboard-query-series.json",
        &series,
    );
    let total_request: Value = read_json(&api_dir().join("requests/dashboard-query-total.json"));
    let total: WidgetQueryResponse =
        post_json(&app, cookie, "/dashboards/query", &total_request).await;
    out.add(
        "query_widget",
        &[],
        &[],
        "responses/dashboard-query-total.json",
        &total,
    );

    // Checked against the static files, not written.
    let schema: SchemaResponse = fetch(&app, cookie, "/schema").await;
    let static_schema: SchemaResponse = read_json(&api_dir().join("static/schema.json"));
    assert_eq!(
        schema, static_schema,
        "static/schema.json differs from GET /schema"
    );
    out.manifest.push(example(
        "get_schema",
        BTreeMap::new(),
        BTreeMap::new(),
        "static/schema.json",
        "application/json",
    ));

    let media_path = format!("/api/v1/projects/{EXAMPLE_PROJECT}/media/{MEDIA}");
    let media = app
        .request(Method::GET, &media_path)
        .header("cookie", cookie)
        .send()
        .await;
    assert_eq!(media.status, StatusCode::OK, "{media_path}");
    assert_eq!(
        media.header("content-type"),
        Some("image/png"),
        "{media_path}"
    );
    let media_file = format!("media/{MEDIA}.png");
    let media_bytes = fs::read(api_dir().join(&media_file))
        .unwrap_or_else(|error| panic!("{media_file}: {error}"));
    assert_eq!(
        media.body.as_ref(),
        media_bytes.as_slice(),
        "{media_path} differs from {media_file}"
    );
    out.manifest.push(example(
        "get_media",
        params(&[("sha256", MEDIA)]),
        BTreeMap::new(),
        &media_file,
        "image/png",
    ));

    // Static entries, copied from the hand-written list this generator replaces.
    out.manifest.extend([
        // The create answers are the versions the read answers above already serve.
        example_created(
            "post_prompt",
            BTreeMap::new(),
            &format!("responses/prompts/{EXAMPLE_PROMPT}/versions/1.json"),
        ),
        example_created(
            "post_prompt_version",
            params(&[("prompt", EXAMPLE_PROMPT)]),
            &format!("responses/prompts/{EXAMPLE_PROMPT}/versions/2.json"),
        ),
        // The label answer is the prompt, as the read serves it: `staging` already points at 2.
        example(
            "put_prompt_label",
            params(&[("prompt", EXAMPLE_PROMPT), ("label", "staging")]),
            BTreeMap::new(),
            &format!("responses/prompts/{EXAMPLE_PROMPT}.json"),
            "application/json",
        ),
        example(
            "put_prompt_settings",
            BTreeMap::new(),
            BTreeMap::new(),
            "responses/prompt-settings.json",
            "application/json",
        ),
        // The create answer has the shape of the read, so the read's file serves it too.
        example_created(
            "post_dataset",
            BTreeMap::new(),
            &format!("responses/datasets/{EXAMPLE_DATASET}.json"),
        ),
        example(
            "post_dataset_import_preview",
            params(&[("dataset", EXAMPLE_DATASET)]),
            params(&[("format", "csv")]),
            "static/dataset-import-preview.json",
            "application/json",
        ),
        example_created(
            "post_dataset_import",
            params(&[("dataset", EXAMPLE_DATASET)]),
            "static/dataset-import-result.json",
        ),
        example_created(
            "post_dataset_item_from_trace",
            params(&[("dataset", EXAMPLE_DATASET)]),
            "static/dataset-item-from-trace.json",
        ),
        example(
            "patch_dataset_items",
            params(&[("dataset", EXAMPLE_DATASET)]),
            BTreeMap::new(),
            "static/dataset-items-edited.json",
            "application/json",
        ),
        example(
            "post_dataset_items_delete",
            params(&[("dataset", EXAMPLE_DATASET)]),
            BTreeMap::new(),
            "static/dataset-items-deleted.json",
            "application/json",
        ),
        example_created(
            "post_dataset_items_from_traces",
            params(&[("dataset", EXAMPLE_DATASET)]),
            "static/dataset-items-from-traces.json",
        ),
        example(
            "restore_dataset",
            params(&[("dataset", EXAMPLE_DATASET)]),
            BTreeMap::new(),
            "static/dataset-restored.json",
            "application/json",
        ),
        example(
            "list_admin_users",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/admin-users.json",
            "application/json",
        ),
        example_created(
            "post_admin_user",
            BTreeMap::new(),
            "static/admin-user-created.json",
        ),
        example(
            "get_admin_settings",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/admin-settings.json",
            "application/json",
        ),
        example(
            "get_admin_license",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/admin-license.json",
            "application/json",
        ),
        example(
            "get_auth_config",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/auth-config.json",
            "application/json",
        ),
        example_status("post_setup", "static/setup-response.json", 201),
        example(
            "post_login",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/me.json",
            "application/json",
        ),
        example_status("post_signup", "static/signup-response.json", 201),
        example(
            "get_me",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/me.json",
            "application/json",
        ),
        example(
            "list_orgs",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/orgs.json",
            "application/json",
        ),
        example(
            "get_org",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/org.json",
            "application/json",
        ),
        example(
            "get_org_entitlements",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/org-entitlements.json",
            "application/json",
        ),
        example(
            "get_org_sso",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/org-sso.json",
            "application/json",
        ),
        example(
            "get_org_usage",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/org-usage.json",
            "application/json",
        ),
        example(
            "list_org_members",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/org-members.json",
            "application/json",
        ),
        example(
            "list_org_invites",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/invites.json",
            "application/json",
        ),
        example_created(
            "post_org_invite",
            params(&[("org", "acme")]),
            "static/invite-created.json",
        ),
        example(
            "get_invite_preview",
            params(&[("token", "EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXA")]),
            BTreeMap::new(),
            "static/invite-preview.json",
            "application/json",
        ),
        example(
            "list_project_members",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/project-members.json",
            "application/json",
        ),
        example(
            "list_projects",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/projects.json",
            "application/json",
        ),
        example(
            "get_project",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/project.json",
            "application/json",
        ),
        example(
            "list_api_keys",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/api-keys.json",
            "application/json",
        ),
        example(
            "list_channels",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/channels.json",
            "application/json",
        ),
        example(
            "list_alert_rules",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/alert-rules.json",
            "application/json",
        ),
        example(
            "get_alert_rule",
            params(&[("id", "01900000-0000-7000-8000-000000000510")]),
            BTreeMap::new(),
            "static/alert-rule.json",
            "application/json",
        ),
        // The other two rules, so the mocked list can open each in its panel.
        example(
            "get_alert_rule",
            params(&[("id", "01900000-0000-7000-8000-000000000511")]),
            BTreeMap::new(),
            "static/alert-rule-0511.json",
            "application/json",
        ),
        example(
            "get_alert_rule",
            params(&[("id", "01900000-0000-7000-8000-000000000512")]),
            BTreeMap::new(),
            "static/alert-rule-0512.json",
            "application/json",
        ),
        example(
            "list_alert_events",
            params(&[("id", "01900000-0000-7000-8000-000000000510")]),
            BTreeMap::new(),
            "static/alert-events.json",
            "application/json",
        ),
        // The other two rules have no events yet.
        example(
            "list_alert_events",
            params(&[("id", "01900000-0000-7000-8000-000000000511")]),
            BTreeMap::new(),
            "static/alert-events-empty.json",
            "application/json",
        ),
        example(
            "list_alert_events",
            params(&[("id", "01900000-0000-7000-8000-000000000512")]),
            BTreeMap::new(),
            "static/alert-events-empty.json",
            "application/json",
        ),
        example(
            "list_project_alert_events",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/project-alert-events.json",
            "application/json",
        ),
        example_status("post_api_key", "static/api-key-created.json", 201),
        example(
            "list_org_model_prices",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/org-model-prices.json",
            "application/json",
        ),
        example(
            "list_llm_connections",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/llm-connections.json",
            "application/json",
        ),
        example(
            "get_llm_connection",
            params(&[
                ("org", "acme"),
                ("id", "01900000-0000-7000-8000-000000000400"),
            ]),
            BTreeMap::new(),
            "static/llm-connection.json",
            "application/json",
        ),
        example_created(
            "post_llm_connection",
            params(&[("org", "acme")]),
            "static/llm-connection.json",
        ),
        example(
            "post_llm_connection_test",
            params(&[
                ("org", "acme"),
                ("id", "01900000-0000-7000-8000-000000000400"),
            ]),
            BTreeMap::new(),
            "static/llm-connection-test.json",
            "application/json",
        ),
        example(
            "list_evaluators",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/evaluators.json",
            "application/json",
        ),
        example(
            "get_evaluator",
            params(&[("id", "01900000-0000-7000-8000-000000000500")]),
            BTreeMap::new(),
            "static/evaluator.json",
            "application/json",
        ),
        example_created("post_evaluator", BTreeMap::new(), "static/evaluator.json"),
        example(
            "list_evaluator_versions",
            params(&[("id", "01900000-0000-7000-8000-000000000500")]),
            BTreeMap::new(),
            "static/evaluator-versions.json",
            "application/json",
        ),
        example(
            "list_evaluator_templates",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/evaluator-templates.json",
            "application/json",
        ),
        example(
            "get_evaluator_stats",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/evaluator-stats.json",
            "application/json",
        ),
        example(
            "post_evaluator_test",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/evaluator-test-result.json",
            "application/json",
        ),
        example(
            "list_score_definitions",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/score-definitions.json",
            "application/json",
        ),
        example_created(
            "post_score_definition",
            BTreeMap::new(),
            "static/score-definition.json",
        ),
        example(
            "list_experiments",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/experiments.json",
            "application/json",
        ),
        example(
            "get_experiment",
            params(&[("id", "01900000-0000-7000-8000-000000000e01")]),
            BTreeMap::new(),
            "static/experiment.json",
            "application/json",
        ),
        example_created(
            "post_experiment",
            BTreeMap::new(),
            "static/experiment-created.json",
        ),
        example(
            "get_experiment_results",
            params(&[("id", "01900000-0000-7000-8000-000000000e01")]),
            BTreeMap::new(),
            "static/experiment-results.json",
            "application/json",
        ),
        example(
            "compare_experiments",
            BTreeMap::new(),
            params(&[(
                "ids",
                "01900000-0000-7000-8000-000000000e01,01900000-0000-7000-8000-000000000e02",
            )]),
            "static/experiment-comparison.json",
            "application/json",
        ),
        example(
            "get_experiment_summaries",
            BTreeMap::new(),
            params(&[(
                "ids",
                "01900000-0000-7000-8000-000000000e02,01900000-0000-7000-8000-000000000e01",
            )]),
            "static/experiment-summaries.json",
            "application/json",
        ),
        example(
            "list_models",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/model-prices.json",
            "application/json",
        ),
        example(
            "resolve_model_price",
            BTreeMap::new(),
            params(&[("model", "gpt-4o"), ("provider", "openai")]),
            "static/model-price-resolution.json",
            "application/json",
        ),
        example_created(
            "post_model_price",
            BTreeMap::new(),
            "static/model-price.json",
        ),
        example(
            "list_personal_tokens",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/personal-tokens.json",
            "application/json",
        ),
        example_status(
            "post_personal_token",
            "static/personal-token-created.json",
            201,
        ),
        example(
            "list_saved_views",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/saved-views.json",
            "application/json",
        ),
        example(
            "get_saved_view",
            params(&[("id", "01900000-0000-7000-8000-000000000211")]),
            BTreeMap::new(),
            "static/saved-view.json",
            "application/json",
        ),
        example(
            "list_dashboards",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/dashboards.json",
            "application/json",
        ),
        example(
            "get_dashboard",
            params(&[("id", "01900000-0000-7000-8000-000000000f01")]),
            BTreeMap::new(),
            "static/dashboard.json",
            "application/json",
        ),
        example_created("create_dashboard", BTreeMap::new(), "static/dashboard.json"),
        example(
            "update_dashboard",
            params(&[("id", "01900000-0000-7000-8000-000000000f01")]),
            BTreeMap::new(),
            "static/dashboard.json",
            "application/json",
        ),
        Example {
            operation_id: "update_dashboard".to_owned(),
            path_params: params(&[("id", "01900000-0000-7000-8000-000000000f01")]),
            query: BTreeMap::new(),
            status: 409,
            content_type: "application/problem+json".to_owned(),
            file: "static/dashboard-stale.json".to_owned(),
        },
        example(
            "get_overview",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/dashboard-overview.json",
            "application/json",
        ),
        example(
            "save_overview",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/dashboard-overview.json",
            "application/json",
        ),
        example(
            "list_annotation_queues",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/annotation-queues.json",
            "application/json",
        ),
        example(
            "get_annotation_queue",
            params(&[("id", "01900000-0000-7000-8000-000000000a01")]),
            BTreeMap::new(),
            "static/annotation-queue.json",
            "application/json",
        ),
        example_created(
            "create_annotation_queue",
            BTreeMap::new(),
            "static/annotation-queue.json",
        ),
        example(
            "list_annotation_queue_items",
            params(&[("id", "01900000-0000-7000-8000-000000000a01")]),
            BTreeMap::new(),
            "static/annotation-queue-items.json",
            "application/json",
        ),
        example(
            "get_annotation_queue_item",
            params(&[
                ("id", "01900000-0000-7000-8000-000000000a01"),
                ("item_id", "01900000-0000-7000-8000-000000000a11"),
            ]),
            BTreeMap::new(),
            "static/annotation-queue-item.json",
            "application/json",
        ),
        example(
            "next_annotation_queue_item",
            params(&[("id", "01900000-0000-7000-8000-000000000a01")]),
            BTreeMap::new(),
            "static/annotation-queue-item.json",
            "application/json",
        ),
        example_created(
            "add_annotation_queue_items",
            params(&[("id", "01900000-0000-7000-8000-000000000a01")]),
            "static/annotation-queue-items-added.json",
        ),
        example(
            "preview_annotation_queue_matching",
            params(&[("id", "01900000-0000-7000-8000-000000000a01")]),
            BTreeMap::new(),
            "static/annotation-queue-matching-preview.json",
            "application/json",
        ),
        example_created(
            "add_annotation_queue_matching",
            params(&[("id", "01900000-0000-7000-8000-000000000a01")]),
            "static/annotation-queue-matching-added.json",
        ),
        example(
            "list_deletions",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/deletions.json",
            "application/json",
        ),
        example(
            "get_deletion",
            params(&[("id", "01900000-0000-7000-8000-000000000902")]),
            BTreeMap::new(),
            "static/deletion.json",
            "application/json",
        ),
        example_status("preview_deletion", "static/deletion-preview.json", 200),
        example_status("create_deletion", "static/deletion-resolving.json", 202),
        example(
            "list_org_deletions",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/org-deletions.json",
            "application/json",
        ),
        example_accepted(
            "delete_project",
            BTreeMap::new(),
            "static/deletion-project.json",
        ),
        example_accepted(
            "delete_org",
            params(&[("org", "acme")]),
            "static/deletion-org.json",
        ),
        example(
            "list_online_rules",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/online-rules.json",
            "application/json",
        ),
        example(
            "get_online_rule_stats",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/online-rule-stats.json",
            "application/json",
        ),
        example(
            "get_online_rule",
            params(&[("id", "01900000-0000-7000-8000-000000000700")]),
            BTreeMap::new(),
            "static/online-rule.json",
            "application/json",
        ),
        // The second rule, so the mocked list can open it in its panel.
        example(
            "get_online_rule",
            params(&[("id", "01900000-0000-7000-8000-000000000701")]),
            BTreeMap::new(),
            "static/online-rule-0701.json",
            "application/json",
        ),
        example_created(
            "post_online_rule",
            BTreeMap::new(),
            "static/online-rule.json",
        ),
        example(
            "post_online_rule_resume",
            params(&[("id", "01900000-0000-7000-8000-000000000701")]),
            BTreeMap::new(),
            "static/online-rule-resumed.json",
            "application/json",
        ),
        example(
            "post_online_rule_preview",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/online-rule-preview.json",
            "application/json",
        ),
        example(
            "list_eval_jobs",
            BTreeMap::new(),
            BTreeMap::new(),
            "static/eval-jobs.json",
            "application/json",
        ),
        example(
            "get_eval_job",
            params(&[("id", "01900000-0000-7000-8000-000000000800")]),
            BTreeMap::new(),
            "static/eval-job.json",
            "application/json",
        ),
        Example {
            status: 202,
            ..example(
                "post_evaluator_run",
                params(&[("id", "01900000-0000-7000-8000-000000000500")]),
                BTreeMap::new(),
                "static/eval-job-queued.json",
                "application/json",
            )
        },
        example(
            "list_backfills",
            params(&[("id", "01900000-0000-7000-8000-000000000700")]),
            BTreeMap::new(),
            "static/backfills.json",
            "application/json",
        ),
        example(
            "list_backfills",
            params(&[("id", "01900000-0000-7000-8000-000000000701")]),
            BTreeMap::new(),
            "static/backfills.json",
            "application/json",
        ),
        example(
            "post_backfill_estimate",
            params(&[("id", "01900000-0000-7000-8000-000000000700")]),
            BTreeMap::new(),
            "static/backfill-estimate.json",
            "application/json",
        ),
        example_accepted(
            "post_backfill",
            params(&[("id", "01900000-0000-7000-8000-000000000700")]),
            "static/backfill.json",
        ),
    ]);
    out.manifest.sort_by(|left, right| {
        left.operation_id
            .cmp(&right.operation_id)
            .then(left.file.cmp(&right.file))
    });
    let Output {
        mut files,
        manifest,
    } = out;
    files.insert("manifest.json".to_owned(), canonical_json(&manifest));
    (files, manifest)
}

fn examples_write_requested() -> bool {
    match std::env::var("CETRA_API_EXAMPLES_WRITE") {
        Err(std::env::VarError::NotPresent) => false,
        Ok(value) if value == "1" => true,
        Ok(value) if value.is_empty() => false,
        Ok(_) | Err(_) => panic!("set CETRA_API_EXAMPLES_WRITE=1 to write, or unset it"),
    }
}

fn on_disk_files() -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    collect_dir(&api_dir().join("responses"), "responses", &mut files);
    let manifest = api_dir().join("manifest.json");
    if manifest.is_file() {
        let text = fs::read_to_string(&manifest)
            .unwrap_or_else(|error| panic!("{}: {error}", manifest.display()));
        files.insert("manifest.json".to_owned(), text);
    }
    let sql_result = api_dir().join("static/sql-result.json");
    if sql_result.is_file() {
        let text = fs::read_to_string(&sql_result)
            .unwrap_or_else(|error| panic!("{}: {error}", sql_result.display()));
        files.insert("static/sql-result.json".to_owned(), text);
    }
    files
}

fn collect_dir(dir: &Path, prefix: &str, files: &mut BTreeMap<String, String>) {
    if !dir.is_dir() {
        return;
    }
    let entries = fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("{error}"));
        let path = entry.path();
        let name = entry.file_name();
        let name = name
            .to_str()
            .unwrap_or_else(|| panic!("{} is not utf-8", path.display()));
        if path.is_dir() {
            collect_dir(&path, &format!("{prefix}/{name}"), files);
        } else {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            files.insert(format!("{prefix}/{name}"), text);
        }
    }
}

fn write_files(files: &BTreeMap<String, String>) {
    let responses = api_dir().join("responses");
    if responses.exists() {
        fs::remove_dir_all(&responses).unwrap_or_else(|error| panic!("{error}"));
    }
    for (file, text) in files {
        let path = api_dir().join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("{}: {error}", parent.display()));
        }
        fs::write(&path, text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn examples_are_up_to_date() {
    let (expected, _manifest) = generate().await;
    if examples_write_requested() {
        write_files(&expected);
        return;
    }
    let actual = on_disk_files();
    for file in expected.keys() {
        let Some(text) = actual.get(file) else {
            panic!("{file} is missing; run `just api-examples`");
        };
        assert_eq!(
            text, &expected[file],
            "{file} differs; run `just api-examples`"
        );
    }
    for file in actual.keys() {
        assert!(
            expected.contains_key(file),
            "{file} is stale; run `just api-examples`"
        );
    }
}
