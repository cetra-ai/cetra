# AS M2 · Tools — plan

Track AS milestone M2 (design: [`00-design.md`](00-design.md) § Tools, D-AS-5, D-AS-6, D-AS-7, D-AS-16). It builds the assistant's tool registry in the server: the six project tools of H's MCP server called in process with the project injected, thirteen new read tools over functions shared with their HTTP routes, and five draft tools that validate with the saving routes' validators and return a draft instead of writing. Nothing calls a model yet (M3). Haiku implements the phases one at a time; each ends with `just check-rust` green and one commit.

**Prerequisites:** M1 merged into this branch (the plan stacks on `claude/as-m1`): M2 uses `crate::api::assistant::AssistantDraftKind` and the `crate::assistant` module that M1 creates.

**Read first:**
- `AGENTS.md`;
- `docs/plans/as-assistant/00-design.md` § Tools, D-AS-5, D-AS-6, D-AS-7;
- `crates/server/src/mcp/` (all of it: `mod.rs` dispatch `run_tool`, `tools.rs` `tool_list`, `arguments`, `PROJECT_ARG`; `output.rs` `max_chars`, `cut`, `fit_list`, `render`; `sql.rs` `query_sql`, `required_window`; `traces.rs` `search_traces`, `get_trace`, `get_span`, `cut_values`, `VALUE_CUT`, `CUT_NOTE`);
- `crates/server/src/http/access.rs` `user_project`; `crates/server/src/http/error.rs` `ApiError`, `problem()`;
- `crates/server/src/routes/read.rs` (`open_query`, `open_query_windowed`, `window`, `list_scope`, `check_search`);
- the route modules this plan extracts from: `routes/sessions.rs`, `routes/scores.rs`, `routes/score_definitions.rs`, `routes/projects.rs`, `routes/alert_rules.rs`, `routes/evaluators.rs`, `routes/online_rules.rs`, `routes/dashboards.rs`;
- `crates/server/src/eval/evaluator.rs` (`prepare`), `crates/server/src/eval/rule_check.rs`;
- `crates/storage/src/reads/mod.rs` (`validate_filter`), `crates/storage/src/sql/mod.rs` (`prepare_sql`), `crates/storage/src/sql/run.rs` (`SqlColumn`, `execute`);
- the code-facts note `as-m2-code-facts.md` in the session notes folder, if present (a survey of the same files).

## What it is, and what it is not

When M2 is done, `crate::assistant::tools` exposes:
- `specs()`: the 24 tools the model sees, each with a name, a description and a JSON schema without any `project` argument;
- `run(state, scope, name, arguments)`: runs one tool for one user in one project and answers a `ToolResult`: a compact JSON value for the model (fitted to 20,000 characters, strings cut, `notes` naming every cut), whether it failed, a row count, and for draft tools the validated draft.

Every tool checks the user's access to the project itself, with the permission of the route that serves the same data, so a tool never reads more than its user could through the API. `/mcp` keeps its seven tools, their schemas and their behaviour; its tests pass unchanged. Each route whose body moves into a shared function keeps its behaviour; its tests pass unchanged.

Out of scope: calling a model, the turn route, streaming, storing messages, limits per user (M3); the panel and Apply (M4). Draft ids (`draft_1`, …) are assigned by M3 when it stores the draft.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AS-5 | MCP's six project functions reused with `project` injected; new reads share one function with their route and use its `ProjectPerm`; MCP's output rules |
| D-AS-6 | Draft tools validate with the saving route's validators and never write; evaluator drafts are judge or check, never webhook |
| D-AS-7 | Tool results are data; no tool writes or reaches the network |
| D-AS-11 | `cetra-assistant` traces carry no content; reaching them through SQL or a filter naming `environment` is accepted |
| D-AS-16 | Every read goes through `open_query`/`open_query_windowed` (the data window) |
| D-AS-19 | Config-reading tools (`get_evaluator`, `get_online_rule`, `get_dashboard`) cut strings at 4,000 characters instead of 200; every other new read tool cuts at 200 like MCP |
| D-AS-20 | Tool limits: list tools take `limit` 1–100 (default 20; `list_evaluators` 1–50); `query_metrics` needs `from` and `to` at most 30 days apart; every result is fitted to 20,000 characters and the model cannot raise `max_chars` |

D-AS-19 and D-AS-20 are added to the design and to `docs/mvp-decisions.md` by AS2.2, before any tool relies on them. The design's § Tools table is also corrected there: the draft tools check `View` on the project (they read it to validate), not "none".

Plan-level choices (no design change):
- `get_session` returns each turn's input and output cut to 200 characters; the model calls `get_trace`/`get_span` for full payloads.
- A draft tool answers the model with `{"accepted": true, "kind": …, "draft": <the normalized draft>}`; M3 turns it into a stored draft part.
- The registry is unused outside tests until M3: `assistant/tools/mod.rs` starts with `#![cfg_attr(not(test), allow(dead_code))] // used by the turn loop (M3); M3 removes this line`, which covers its child modules.

## Versions

No new dependency.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| AS2.1 Storage checks | `crates/storage/src/reads/mod.rs`, `crates/storage/src/sql/run.rs`, `docs/spec/05-query.md` | — |
| AS2.2 Registry and MCP tools | `crates/server/src/assistant/mod.rs`, `crates/server/src/assistant/tools/mod.rs` (new), `crates/server/src/assistant/tools/shape.rs` (new), `crates/server/src/mcp/mod.rs`, `crates/server/src/mcp/tools.rs`, `crates/server/src/mcp/output.rs`, `docs/plans/as-assistant/00-design.md`, `docs/mvp-decisions.md` (D-AS-19 and D-AS-20 rows only) | — |
| AS2.3 Reads: sessions, scores, project, alerts | `crates/server/src/assistant/tools/mod.rs`, `crates/server/src/assistant/tools/shape.rs`, `crates/server/src/assistant/tools/reads.rs` (new), `routes/sessions.rs`, `routes/scores.rs`, `routes/score_definitions.rs`, `routes/projects.rs`, `routes/alert_rules.rs` (all under `crates/server/src/`) | AS2.2 |
| AS2.4 Reads: evaluation and dashboards | `crates/server/src/assistant/tools/mod.rs`, `crates/server/src/assistant/tools/reads.rs`, `routes/evaluators.rs`, `routes/online_rules.rs`, `routes/dashboards.rs` | AS2.3 |
| AS2.5 Drafts | `crates/server/src/assistant/tools/mod.rs`, `crates/server/src/assistant/tools/drafts.rs` (new), `routes/dashboards.rs`, `routes/evaluators.rs`, `routes/online_rules.rs` | AS2.1, AS2.4 |

All phases run in order in the same worktree.

**Checks for every phase.** Do not build or test while editing. Before the commit, run once:

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target just check-rust
```

**Shared rules for every phase.**
- Moving a route's body into a shared function: the handler keeps its extractors, its `RouteDef` and its answer; it becomes `Ok(error::Json(shared(&state, &access, params).await?))` (or the equivalent). Helpers the body uses stay in the route module, private unless the plan says `pub(crate)`. No behaviour change: the module's existing tests must pass unchanged.
- A tool never logs its arguments or its result (`AGENTS.md` § Privacy).
- New tests call the tools through `run` with an `AuthUser { id, is_instance_admin: false, email }` (never `true`: an instance admin is not a stand-in for a member), on `TestApp::new()`, `app.seed()` and `app.load_example_dataset(seeded.project.id)`. The example dataset holds spans, traces, sessions (`sess-001`, `sess-002`) and scores in storage, all in 2026-01-01 (`mcp::test_helpers::FROM`/`TO`); it holds **no** evaluator, online rule, dashboard, score definition, alert rule or channel in `meta.db`. Tests that need those create them first through the HTTP API with the seeded cookie (`app.request(Method::POST, "/api/v1/projects/{project}/…").header("cookie", &seeded.cookie).json(&body).send().await`), copying the request bodies the route module's own tests use. Use a `check` evaluator (it needs no LLM connection) wherever an evaluator is needed. A second org is `crate::test_support::second_org(&app, slug, owner_email)`, which returns `(OrgId, Member)` and creates no project; its owner (`AuthUser { id: member.id, is_instance_admin: false, email: owner_email }`) is a user who cannot read the seeded project. A viewer of the seeded project is `app.add_member(&seeded, "viewer@example.com", OrgRole::Viewer)` (`Member` has `id` and `cookie`; the email is the one you passed).
- If clippy reports `large_futures` in `run`, wrap the offending tool call in `Box::pin(…)`.
- `Limit` (`api/common.rs`) has a private field and no constructor: build one with `serde_json::from_value::<Limit>(serde_json::json!(n))` (it cannot fail for 1–1,000; propagate the error as `ApiError::BadRequest` anyway, never `unwrap`).

---

### AS2.1 Storage checks

**Goal:** storage can check a sessions or scores filter, and plan a SQL statement without running it.

**Edit:** `crates/storage/src/reads/mod.rs`, `crates/storage/src/sql/run.rs`, `docs/spec/05-query.md`. (`SqlColumn` is already re-exported from the storage crate root.)

**Steps:**

1. `validate_filter` (`reads/mod.rs`): add the arms `ViewName::Sessions => FilterView::Sessions` and `ViewName::Scores => FilterView::Scores`. Any other `ViewName` keeps the `InvalidRequest` answer, with its message changed to `"filters are checked for traces, spans, sessions and scores only"`. Update the doc comment's `# Errors` to match. In the existing test `validate_filter_accepts_blank_and_rejects_bad_text` (same file), delete the last assertion (the one expecting `ViewName::Sessions` to be `RunError::InvalidRequest`); the new tests below cover sessions.

2. `sql/run.rs`, in the `impl QueryContext` block that holds `sql_json`, add:

   ```rust
   /// Plans `query` with `params` bound, without running it, and returns its output columns (AS M2, D-AS-6).
   ///
   /// # Errors
   ///
   /// [`RunError::Invalid`] when the statement does not parse, is not one read-only query, or names
   /// an unknown table or column; [`RunError::InvalidRequest`] for a parameter value that does not fit
   /// its placeholder.
   pub async fn sql_check(&self, query: &str, params: &Map<String, Value>) -> Result<Vec<SqlColumn>, RunError> {
       let plan = prepare_sql(self, query, params).await?;
       Ok(plan
           .schema()
           .fields()
           .iter()
           .map(|field| SqlColumn { name: field.name().to_owned(), data_type: field.data_type().to_string() })
           .collect())
   }
   ```

   Import what is missing (`prepare_sql` is already used by `execute` in the same file).

3. `docs/spec/05-query.md`: at the end of the filter DSL section, add the sentence: "A filter can be checked without running for the traces, spans, sessions and scores views; the SQL console's statement can be planned without running (`sql_check`), which the assistant's drafts use (D-AS-6)."

**Tests** (in the storage crate, next to the existing `validate_filter` and `sql_json` tests; read them for the context fixture they use):
- `validate_filter_accepts_sessions_and_scores`: a valid sessions filter (`trace_count > 1`) and a valid scores filter (`name = 'x'`) are `Ok`; an unknown column in each is `RunError::Invalid` with a position.
- `validate_filter_refuses_other_views`: `ViewName::ExperimentItems` is `InvalidRequest` with the new message.
- `sql_check_returns_columns_without_running`: `SELECT name, count(*) AS n FROM traces GROUP BY name` returns the two columns with their Arrow types.
- `sql_check_rejects_a_bad_statement`: `DELETE FROM traces`, `SELECT nope FROM traces` and `SELEC 1` are `RunError::Invalid`; the last two carry a position (the first may not).
- `sql_check_binds_parameters`: `SELECT {n:Int} AS x` with `{"n": 3}` is `Ok`; with `{"n": "abc"}` is `RunError::InvalidRequest`; with no `n` is the same error `sql_json` gives.

**Done when:** `just check-rust` passes.

**Commit:** `Check sessions and scores filters and plan SQL without running it`

---

### AS2.2 Registry and MCP tools

**Goal:** the registry with its types and output shaping, serving the six MCP project tools with the project injected.

**Edit:** `crates/server/src/assistant/mod.rs`, `crates/server/src/assistant/tools/mod.rs` (new), `crates/server/src/assistant/tools/shape.rs` (new), `crates/server/src/mcp/mod.rs`, `crates/server/src/mcp/tools.rs`, `crates/server/src/mcp/output.rs`, `docs/plans/as-assistant/00-design.md`, `docs/mvp-decisions.md` (two rows).

**Steps:**

1. `crates/server/src/mcp/mod.rs`: change `mod output; mod sql; mod tools; mod traces;` to `pub(crate) mod …` for all four. Nothing else in the file changes.

2. `crates/server/src/mcp/tools.rs`: move each tool's JSON schema out of `tool_list()` into a `pub(crate) fn <tool>_schema() -> Value` (`get_schema_schema`, `query_sql_schema`, `list_saved_views_schema`, `search_traces_schema`, `get_trace_schema`, `get_span_schema`, `list_projects_schema`), returning the exact `json!` it returns today, and make each description a `pub(crate) const <TOOL>_DESCRIPTION: &str` (`GET_SCHEMA_DESCRIPTION`, …). `tool_list()` calls them, so `tools/list` answers byte-for-byte the same (the test `tools_list_has_every_tool_read_only` checks it).

3. `crates/server/src/assistant/mod.rs`: add `pub(crate) mod tools;` (M1 created this file).

4. `crates/server/src/assistant/tools/shape.rs`: output shaping shared by the new tools.

   ```rust
   //! Output rules of the assistant's tools: MCP's (D-H-11), applied to API values (D-AS-5).

   use serde_json::{Map, Value};

   /// The character budget of one tool result (MCP's default, D-H-10).
   pub(crate) const MAX_CHARS: usize = crate::mcp::output::DEFAULT_MAX_CHARS;
   /// Strings longer than this are cut in read results (MCP's `VALUE_CUT`).
   pub(crate) const VALUE_CUT: usize = 200;
   /// The cut for tools that return configuration the model may redraft (D-AS-19).
   pub(crate) const CONFIG_CUT: usize = 4_000;

   /// Cuts every string inside `value`, at any depth (object values and array items; never object keys),
   /// to `max` characters with `crate::mcp::output::cut`. Returns true when anything was cut.
   pub(crate) fn cut_strings(value: &mut Value, max: usize) -> bool;

   /// Builds `{<list>: items, "next_cursor": cursor, "truncated": false, "notes": []}`. It cuts the strings
   /// of the items only (never `next_cursor` or `notes`) to `cut`, adding the note "string values longer than
   /// {cut} characters were cut to {cut} characters", then fits the body to [`MAX_CHARS`] with
   /// `crate::mcp::output::fit_list` (note: "the list was cut to fit {MAX_CHARS} characters; ask for fewer rows").
   pub(crate) fn list_body(list: &str, items: Vec<Value>, cursor: Option<String>, cut: usize) -> Value;

   /// One object result. `value` must be a JSON object (callers serialize a struct). It cuts the object's
   /// strings to `cut`; when `list` names an array member (`turns`, `series`, `rows`) it then fits that array
   /// with `fit_list`; when the compact JSON is still longer than [`MAX_CHARS`], it re-cuts with `cut/2`,
   /// `cut/4`, … down to 50 characters, then adds the note "the result is larger than {MAX_CHARS} characters
   /// even with values cut to 50 characters" and keeps it. Adds `"truncated"` and `"notes"` keys.
   pub(crate) fn object_body(value: Value, cut: usize, list: Option<&str>) -> Value;
   ```

   `list_body` sets `"truncated": true` when `next_cursor` is some, or when `fit_list` dropped items. `object_body` sets `truncated` to the object's own `truncated` (false when absent) OR anything was cut or dropped, and extends an existing `notes` array instead of replacing it (`SessionDetail` already has `truncated`). Measure with `crate::mcp::output::char_len` of `crate::mcp::output::render(&value)`. The 20,000 characters are a best effort for objects (a single list item larger than the budget stays, with the note), and exact for lists.

5. `crates/server/src/assistant/tools/mod.rs`: the registry.

   ```rust
   //! The assistant's tools (track AS M2, D-AS-5, D-AS-6).
   #![cfg_attr(not(test), allow(dead_code))] // used by the turn loop (M3); M3 removes this line

   mod shape;

   use cetra_core::ids::ProjectId;
   use serde_json::{Map, Value};

   use crate::api::assistant::AssistantDraftKind;
   use crate::auth::principal::AuthUser;
   use crate::http::AppState;
   use crate::http::error::ApiError;

   /// A JSON object of tool arguments, as the model sends them.
   pub(crate) type JsonObject = Map<String, Value>;

   /// One tool as the model sees it.
   #[derive(Debug, Clone, PartialEq)]
   pub(crate) struct ToolSpec {
       pub(crate) name: &'static str,
       pub(crate) description: &'static str,
       /// A JSON schema object with no `project` property.
       pub(crate) parameters: Value,
   }

   /// Who calls and where.
   #[derive(Debug, Clone)]
   pub(crate) struct ToolScope {
       pub(crate) user: AuthUser,
       pub(crate) project: ProjectId,
   }

   /// A tool's answer.
   #[derive(Clone, PartialEq)]
   pub(crate) struct ToolResult {
       /// False when the tool failed; `value` is then `{"error": {"code", "message", "position"?}}`.
       pub(crate) ok: bool,
       /// What the model reads.
       pub(crate) value: Value,
       /// Rows or items returned, when the tool returns a list or rows.
       pub(crate) rows: Option<u64>,
       /// A validated draft (draft tools only).
       pub(crate) draft: Option<(AssistantDraftKind, Value)>,
   }

   /// A tool's failure: most tools fail with an `ApiError`; `query_metrics` shares the dashboard cache's
   /// `Arc<ApiError>`, since `ApiError` is not `Clone`.
   #[derive(Debug)]
   pub(crate) enum ToolError {
       Api(ApiError),
       Shared(std::sync::Arc<ApiError>),
   }

   impl From<ApiError> for ToolError {
       fn from(error: ApiError) -> Self { Self::Api(error) }
   }

   /// The model named a tool that does not exist.
   #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
   #[error("unknown tool")]
   pub(crate) struct UnknownTool;

   /// Every tool, in a fixed order.
   pub(crate) fn specs() -> Vec<ToolSpec>;

   /// Runs `name` with `arguments` for `scope`.
   pub(crate) async fn run(state: &AppState, scope: &ToolScope, name: &str, arguments: JsonObject) -> Result<ToolResult, UnknownTool>;
   ```

   `ToolResult` gets a hand-written `Debug` printing `ok`, `rows`, the draft kind, and `value_len` (the compact JSON length), never the value (privacy). Check that `thiserror` is already a dependency of `cetra-server` (it is the crate's error library per `AGENTS.md`); if not, report.

6. In `run`, for the six MCP tools (`get_schema`, `query_sql`, `list_saved_views`, `search_traces`, `get_trace`, `get_span`):
   1. insert `"project"` = `scope.project.to_string()` into `arguments`, overwriting any value the model sent (the model's schema has no `project`, so a sent one is ignored, never trusted);
   2. call `crate::mcp::tools::get_schema(state, &scope.user, arguments)` (and so on);
   3. `Ok(value)` → `ToolResult { ok: true, value, rows, draft: None }`, where `rows` is `value["row_count"]` for `query_sql`, the length of `value["traces"]` for `search_traces`, the length of `value["spans"]` for `get_trace`, the length of `value["views"]` for `list_saved_views`, and `None` for `get_schema` and `get_span`;
   4. `Err(error)` → `ToolResult { ok: false, value: crate::mcp::output::failure_body(&error), rows: None, draft: None }`. Add `pub(crate) fn failure_body(error: &ApiError) -> Value` to `mcp/output.rs` by extracting the body that `failure` builds today (`{"error": {"code", "message", "position"?}}`, including its call to `error.log()`); `failure` then calls it, so the two never differ. For a `ToolError` (AS2.3 on), `run` passes `&error` for `Api(error)` and `&*arc` for `Shared(arc)`.

   Before step 2, for `query_sql`, `search_traces`, `get_trace` and `get_span` only, also insert `"max_chars": 20000` (overwriting): the model cannot raise the budget (D-AS-20). `get_schema` and `list_saved_views` take no `max_chars` and refuse unknown fields, so nothing is inserted for them.

   An unknown `name` is `Err(UnknownTool)`.

7. `specs()` for the six MCP tools: name, the description constant from step 2, and the schema from step 2 with `"project"` and `"max_chars"` removed from `properties` and from `required` (a `required` left empty is removed), by a private `fn model_schema(schema: Value) -> Value`. Order: `get_schema`, `query_sql`, `list_saved_views`, `search_traces`, `get_trace`, `get_span`. The descriptions mention `list_projects` only in `get_schema`/`PROJECT_ARG` texts; since `PROJECT_ARG` sits inside the removed property, nothing else needs changing. The MCP `query_sql` description says evaluation traces are included: keep it.

8. Docs. In `docs/plans/as-assistant/00-design.md`: add to the decisions table, after D-AS-18,
   - `| D-AS-19 | **Config tools cut at 4,000 characters.** \`get_evaluator\`, \`get_online_rule\` and \`get_dashboard\` cut strings at 4,000 characters instead of MCP's 200, so the model can read and redraft a judge template or a widget query; the other read tools cut at 200. | Cutting at 200: the model would redraft a template it cannot read. |`
   - `| D-AS-20 | **Tool limits.** List tools take \`limit\` 1–100 (default 20; \`list_evaluators\` 1–50, the stats read's cap); \`query_metrics\` needs \`from\` and \`to\` at most 30 days apart, like \`query_sql\` (D-H-9); every result is fitted to 20,000 characters and the model cannot raise \`max_chars\`. | The routes' 1,000 rows and open windows: an agent would flood its context or scan without bound. |`

   (written without the backslashes); the tool keeps the design's name `get_project_settings`. In § Tools change the Permission column of the five draft rows to `View` ("they read the project to validate"; `draft_dashboard`'s edit and `draft_online_rule` read more, still under `View`). In `docs/mvp-decisions.md` § Track AS, after the D-AS-18 row: `| D-AS-19 | Config cut | \`get_evaluator\`, \`get_online_rule\` and \`get_dashboard\` cut strings at 4,000 characters; other read tools at 200 like MCP. |` and `| D-AS-20 | Tool limits | Lists 1–100 rows (evaluators 1–50); \`query_metrics\` at most 30 days; results fitted to 20,000 characters, not raisable by the model. |` (again without backslashes).

**Tests** (in `assistant/tools/mod.rs` and `shape.rs` `mod tests`):
- `specs_have_no_project_argument`: no spec's `parameters` has `project` or `max_chars` in `properties` or `required`; names are unique.
- `the_model_cannot_raise_max_chars`: `get_trace` of the example's largest trace with `"max_chars": 100000` answers at most 20,000 characters.
- `mcp_tool_list_is_unchanged`: the existing `mcp::tests::tools_list_has_every_tool_read_only` passes (no new test; list it in the report).
- `query_sql_runs_in_the_scope_project`: `SELECT count(*) AS n FROM traces` with `FROM`/`TO` returns `ok`, `rows == Some(1)`.
- `a_project_argument_from_the_model_is_overwritten`: arguments carrying `"project": Uuid::now_v7().to_string()` still answer from the scope project.
- `a_user_without_access_gets_not_found`: the `second_org` owner's `AuthUser` with the seeded project as scope answers `ok: false` with code `not_found`.
- `errors_carry_the_problem_code`: `query_sql` with `SELECT nope FROM traces` answers `ok: false`, code `invalid_filter`, a `position`.
- `unknown_tool_is_an_error`: `run(…, "drop_everything", …)` is `Err(UnknownTool)`.
- `cut_strings_walks_nested_values`: nested objects and arrays, a 201-character string cut to 200, keys untouched, returns true; a short tree returns false.
- `list_body_fits_and_notes`: 1,000 items of 100 characters → `truncated: true`, the fit note, compact length ≤ `MAX_CHARS`; a cursor alone sets `truncated`; a 300-character cursor is kept whole.
- `object_body_halves_until_it_fits`: a 30-string object of 4,000 characters each fits under `MAX_CHARS` with a smaller cut and a note.
- `object_body_fits_its_list`: `{"turns": [500 objects of 200 characters]}` with `list: Some("turns")` fits and notes the cut.
- `tool_result_debug_hides_the_value`: `format!("{result:?}")` holds no value text.

**Done when:** `just check-rust` passes; the MCP tests pass unchanged.

**Commit:** `Add the assistant tool registry with the MCP read tools (AS M2)`

---

### AS2.3 Reads: sessions, scores, project, alerts

**Goal:** six read tools over functions shared with their routes.

**Edit:** `crates/server/src/assistant/tools/mod.rs`, `crates/server/src/assistant/tools/reads.rs` (new), `crates/server/src/routes/sessions.rs`, `crates/server/src/routes/scores.rs`, `crates/server/src/routes/score_definitions.rs`, `crates/server/src/routes/projects.rs`, `crates/server/src/routes/alert_rules.rs`.

**Steps:**

1. Extract, following the shared rules (signatures exact; each route handler then calls it):

   | Module | New `pub(crate)` function | From handler |
   |---|---|---|
   | `routes/sessions.rs` | `async fn session_page(state: &AppState, access: &ProjectAccess, params: SessionListParams) -> Result<Page<SessionSummary>, ApiError>` | `list_sessions` |
   | `routes/sessions.rs` | `async fn session_detail(state: &AppState, access: &ProjectAccess, session_id: &str, exclusions: ExclusionParams) -> Result<SessionDetail, ApiError>` | `get_session` |
   | `routes/scores.rs` | `async fn score_page(state: &AppState, access: &ProjectAccess, params: ScoreListParams) -> Result<Page<Score>, ApiError>` | `list_scores` |
   | `routes/score_definitions.rs` | `async fn definition_page(state: &AppState, access: &ProjectAccess, params: ScoreDefinitionListParams) -> Result<Page<ScoreDefinition>, ApiError>` | `list_score_definitions` |
   | `routes/projects.rs` | `async fn project_detail(state: &AppState, access: &ProjectAccess) -> Result<Project, ApiError>` | `get_project` (the route handler) |
   | `routes/alert_rules.rs` | `async fn alert_rule_page(state: &AppState, access: &ProjectAccess) -> Result<Page<AlertRule>, ApiError>` | `list_rules` |

   The parameter types are the existing API types (their fields are `pub`); only `Limit` needs the `serde_json::from_value` construction of the shared rules.

2. Add `mod reads;` to `assistant/tools/mod.rs`. `assistant/tools/reads.rs`: one `pub(super) async fn` per tool, each `(state: &AppState, scope: &ToolScope, arguments: JsonObject) -> Result<(Value, Option<u64>), ToolError>`, in this order of operations: (a) parse arguments with `crate::mcp::tools::arguments::<Args>(arguments)?` into a private `#[derive(Deserialize)] #[serde(deny_unknown_fields)]` struct; (b) check bounds (`limit` 1–100, default 20, else `ApiError::BadRequest` with detail `"limit must be between 1 and 100"`); (c) `let access = user_project(state, &scope.user, &scope.project.to_string(), ProjectPerm::View).await?;`; (d) call the shared function; (e) `serde_json::to_value` each item; (f) shape with `shape::list_body`/`object_body` and `VALUE_CUT`; (g) return the value and the item count (`None` for single objects).

   | Tool | Arguments (JSON schema properties; all optional unless marked) | Calls | Result key |
   |---|---|---|---|
   | `list_sessions` | `filter` (string, sessions view filter DSL), `from`, `to` (strings, RFC 3339 or epoch ms), `sort` (`start_time`, `end_time`, `trace_count`, `cost_total`), `order` (`asc`, `desc`), `limit`, `cursor`, `include_eval`, `include_experiments` (booleans) | `session_page` with a `SessionListParams` built from them (`Limit` from `limit`) | `sessions` |
   | `get_session` | `session_id` (string, required), `include_eval`, `include_experiments` | `session_detail`, then `object_body(value, VALUE_CUT, Some("turns"))` (the cut reaches every string of each turn's `input` and `output`) | object |
   | `list_scores` | `target` (`span`, `trace`, `session`), `name`, `source` (`api`, `sdk`, `human`, `end_user`, `llm_judge`, `check`, `webhook`: the serde names of `cetra_core::enums::ScoreSource`; copy the full list from that enum), `trace_id`, `span_id`, `session_id`, `evaluator_id`, `rule_id`, `from`, `to`, `limit`, `cursor` | `score_page` | `scores` |
   | `list_score_definitions` | `include_archived` (boolean), `limit`, `cursor` | `definition_page` | `definitions` |
   | `get_project_settings` | none | `project_detail` | object |
   | `list_alert_rules` | none | `alert_rule_page` | `rules` |

   Build each tool's JSON schema by hand in `reads.rs` as a `fn <tool>_schema() -> Value` (`"type": "object"`, `"additionalProperties": false`, `properties` with a one-line `description` each, `required` where marked). Enum values come from the API types' serde names: read them from `api/sessions.rs`, `api/scores.rs` and `cetra_core::enums` and list them exactly.

3. Descriptions (constants in `reads.rs`), one sentence each, saying what the tool returns and when to use it:
   - `list_sessions`: "Sessions (conversations grouped by session_id) in a time window, newest first, with trace counts, cost, errors and scores. Evaluation and experiment traces are left out unless asked."
   - `get_session`: "One session's turns in order: each trace's summary and its root input and output, cut to 200 characters. Use get_trace for a full turn."
   - `list_scores`: "Scores (human, API and evaluator scores), newest first, filtered by name, source, target or evaluator."
   - `list_score_definitions`: "The project's score definitions: name, type, range or categories, direction."
   - `get_project_settings`: "The project's settings: name, retention, trace completion timeout, last ingest time."
   - `list_alert_rules`: "The project's alert rules with their condition, state, last value and last error."

4. Register the six in `specs()` (after the MCP tools, in the table's order) and in `run`'s `match`, mapping `Err(ToolError)` as AS2.2 step 6.4 says.

**Tests** (in `reads.rs` `mod tests`, on the example dataset):
- `list_sessions_returns_the_example_sessions`: `from`/`to` = the example day; the same session ids in the same order as `GET /sessions` with the same window answers; `rows == Some(2)`.
- `get_session_keeps_the_route_truncated_flag`: the tool's `truncated` is true whenever the route's `SessionDetail.truncated` is (compare with `GET /sessions/sess-001`).
- `list_sessions_rejects_a_bad_filter`: `filter: "nope = 1"` → `ok: false`, code `invalid_filter`.
- `get_session_cuts_payloads`: every string in `get_session("sess-001")`'s answer is at most 200 characters (walk the JSON in the test).
- `get_session_unknown_is_not_found`.
- `list_scores_filters_by_name`: the example scores; a name filter keeps only that name.
- `list_score_definitions_lists_live_ones`: two definitions created through `POST /score-definitions`, one archived through `DELETE`; the tool lists one, and two with `include_archived: true`.
- `get_project_settings_returns_the_settings`: name and retention of the seeded project.
- `list_alert_rules_holds_no_channel_config`: create a webhook channel and an alert rule using it through the API, copying the bodies of `routes/alert_rules.rs`'s and `routes/channels.rs`'s tests; the tool's JSON contains the channel id and not the webhook URL.
- `limit_is_bounded`: `limit: 0` and `limit: 101` → `ok: false`, code `bad_request`.
- `a_project_argument_is_refused`: `list_sessions` with `{"project": "x"}` → `ok: false`, `bad_request`. Unlike the MCP tools, the new tools take no `project`, and `run` inserts none for them.
- `viewer_reads_the_same`: a viewer's `AuthUser` gets `ok: true` on every tool (the reads are `View`; M3's `UseAssistant` keeps viewers out of the turn route, not the functions).
- the moved routes' existing tests pass unchanged.

**Done when:** `just check-rust` passes.

**Commit:** `Add the assistant's session, score, project and alert read tools (AS M2)`

---

### AS2.4 Reads: evaluation and dashboards

**Goal:** seven more read tools: evaluators, online rules, dashboards and the metrics query.

**Edit:** `crates/server/src/assistant/tools/mod.rs`, `crates/server/src/assistant/tools/reads.rs`, `crates/server/src/routes/evaluators.rs`, `crates/server/src/routes/online_rules.rs`, `crates/server/src/routes/dashboards.rs`.

**Steps:**

1. Extract (shared rules apply):

   | Module | New `pub(crate)` function | From handler |
   |---|---|---|
   | `routes/evaluators.rs` | `async fn evaluator_page(state: &AppState, access: &ProjectAccess, params: PageParams) -> Result<Page<Evaluator>, ApiError>` | `list_evaluators` |
   | `routes/evaluators.rs` | `async fn evaluator_detail(state: &AppState, access: &ProjectAccess, id: Uuid) -> Result<Evaluator, ApiError>` | `get_evaluator` |
   | `routes/evaluators.rs` | `async fn evaluator_stats(state: &AppState, access: &ProjectAccess, evaluator_ids: Option<&str>) -> Result<EvaluatorStatsResponse, ApiError>` | `get_evaluator_stats` |
   | `routes/online_rules.rs` | `async fn rule_page(state: &AppState, access: &ProjectAccess, params: PageParams) -> Result<OnlineRulePage, ApiError>` | `list_online_rules` |
   | `routes/online_rules.rs` | `async fn rule_detail(state: &AppState, access: &ProjectAccess, id: Uuid) -> Result<OnlineRule, ApiError>` | `get_online_rule` |
   | `routes/online_rules.rs` | `async fn rule_stats(state: &AppState, access: &ProjectAccess) -> Result<OnlineRuleStatsResponse, ApiError>` | `get_online_rule_stats` |
   | `routes/dashboards.rs` | `async fn dashboard_page(state: &AppState, access: &ProjectAccess, params: DashboardListParams) -> Result<Page<DashboardSummary>, ApiError>` | `list_dashboards` |
   | `routes/dashboards.rs` | `async fn dashboard_detail(state: &AppState, access: &ProjectAccess, id: Uuid) -> Result<Dashboard, ApiError>` | `get_dashboard` |
   | `routes/dashboards.rs` | `async fn widget_data(state: &AppState, access: &ProjectAccess, body: WidgetQueryRequest) -> Result<Arc<WidgetQueryResponse>, Arc<ApiError>>` | `query_widget` |

   For `widget_data`, the route keeps its `SharedError` mapping: `widget_data(&state, &access, body).await.map(|data| error::Json((*data).clone())).map_err(SharedError)`. If `WidgetQueryResponse` is not `Clone`, keep exactly what the handler does today with the `Arc` and report it. The `query_metrics` tool maps its error with `ToolError::Shared(arc)`.

2. Tools in `reads.rs` (same order of operations as AS2.3; permission `View` except `query_metrics`, which uses `ProjectPerm::Query` like its route):

   | Tool | Arguments | Calls | Result | Cut |
   |---|---|---|---|---|
   | `list_evaluators` | `limit` (1–50, default 20), `cursor` | `evaluator_page`; when the page is not empty, `evaluator_stats` with the page's ids joined by commas (never more than 50, the stats read's cap); each item is the evaluator with its `stats` entry, from which `jobs.runs_daily` and `scores.daily` are removed | `evaluators` | `VALUE_CUT` |
   | `get_evaluator` | `evaluator_id` (required) | `evaluator_detail` and `evaluator_stats` for that id (`jobs.runs_daily` and `scores.daily` kept) | object `{evaluator, stats}` | `CONFIG_CUT` |
   | `list_online_rules` | `limit`, `cursor` | `rule_page` then `rule_stats`; each item is the rule (which already holds `jobs_last_24h`) with its `stats` entry, from which `jobs_hourly` and `scores.hourly` are removed | `rules` | `VALUE_CUT` |
   | `get_online_rule` | `rule_id` (required) | `rule_detail` and its entry of `rule_stats` (`jobs_hourly` and `scores.hourly` kept) | object `{rule, stats}` | `CONFIG_CUT` |
   | `list_dashboards` | `limit`, `cursor` | `dashboard_page` | `dashboards` | `VALUE_CUT` |
   | `get_dashboard` | `dashboard_id` (required) | `dashboard_detail` (with `version`, needed to draft an edit) | object | `CONFIG_CUT` |
   | `query_metrics` | the `WidgetQueryRequest` fields (`from`, `to` required RFC 3339 strings (the schema says so; the route refuses epoch milliseconds), at most 30 days apart, checked by parsing them as `TimeParam` and calling `crate::mcp::sql::required_window` before the call; `bucket`, `filter`, `group_by`, `include_eval`, `include_experiments`, `query` (required: `source`, `measures`, `breakdown`, `filter`), `shape` (required), `compare`) | `widget_data`, then `object_body(value, VALUE_CUT, Some("series"))` for the series shape or `Some("rows")` for the total shape | object | `VALUE_CUT` |

   Ids that are not UUIDs are `ApiError::NotFound` (as the routes answer). `query_metrics`' schema describes `query` with the measure aggregations and fields the dashboard builder accepts: copy them from the `MeasureBody`/`BreakdownBody` serde names in `api/dashboards.rs` and spec 05 § Dashboard queries.

3. Descriptions (constants):
   - `list_evaluators`: "Evaluators (LLM judges, checks, webhooks) with their output score and last-7-days job and score summary."
   - `get_evaluator`: "One evaluator's full configuration (judge template, model, connection id, check parameters) and its daily stats. Webhook secrets and URLs are never shown."
   - `list_online_rules`: "Online evaluation rules: evaluator, filter, sampling, budget, state and the last 24 hours of jobs and scores."
   - `get_online_rule`: "One online rule's configuration and its hourly jobs and scores over the last 24 hours."
   - `list_dashboards`: "The project's custom dashboards (name, description)."
   - `get_dashboard`: "One dashboard's definition: its widgets, their queries and layout, and its version."
   - `query_metrics`: "Aggregate traces, spans or scores over time or by group, as a dashboard widget does: counts, sums, averages and percentiles of a field, optionally broken down. from and to are required, at most 30 days apart."

4. Register the seven in `specs()` and `run` (after AS2.3's).

5. Every test of this phase creates its rows first (shared rules): a `check` evaluator (`kind: "check"`, config and output as in `routes/evaluators.rs`'s tests), an online rule over it (body as in `routes/online_rules.rs`'s tests), a dashboard with one widget (body as in `routes/dashboards.rs`'s tests).

**Tests:**
- `list_evaluators_summarizes_stats`: a created check evaluator appears with `stats` and no `scores.daily`; an empty project answers an empty list (no stats call); `limit: 51` is `bad_request`.
- `get_evaluator_keeps_long_config`: a check evaluator whose `regex` pattern (or another string parameter of a check config) is 1,000 characters comes back whole.
- `get_online_rule_and_list`: one rule created through the API appears in both.
- `get_dashboard_returns_the_version`.
- `query_metrics_counts_traces`: arguments `{"from": FROM, "to": TO, "bucket": "auto", "query": {"source": "traces", "measures": [{"agg": "count"}]}, "shape": "total"}` (check the serde names against `api/dashboards.rs`: `AggBody`, `WidgetShape`, `MetricsBucket`) → one row whose value is the example's trace count.
- `query_metrics_requires_a_window`: no `to` → `bad_request`; 31 days → `bad_request`; epoch milliseconds → `bad_request` (refused by the route's RFC 3339 parse inside `widget_data`, not by `required_window`).
- `query_metrics_bad_filter_is_invalid_filter`.
- the moved routes' existing tests pass unchanged.

**Done when:** `just check-rust` passes.

**Commit:** `Add the assistant's evaluation, dashboard and metrics read tools (AS M2)`

---

### AS2.5 Drafts

**Goal:** the five draft tools.

**Edit:** `crates/server/src/assistant/tools/mod.rs`, `crates/server/src/assistant/tools/drafts.rs` (new), `crates/server/src/routes/dashboards.rs`, `crates/server/src/routes/evaluators.rs`, `crates/server/src/routes/online_rules.rs`.

**Steps:**

1. Expose the validators (no behaviour change):
   - `routes/dashboards.rs`: make `checked_name`, `checked_description`, `check_definition`, `refuse_if_any` and `check_expressions` `pub(crate)`.
   - `routes/evaluators.rs`: make `check_against_store` and `output_of` `pub(crate)`.
   - `routes/online_rules.rs`: make `Draft` (rename it `RuleDraft` and update its uses in the module) and its fields `pub(crate)`, and `validate`, `normalize_filter` `pub(crate)`.

2. Add `mod drafts;` to `assistant/tools/mod.rs`. `assistant/tools/drafts.rs`: one `pub(super) async fn` per tool, `(state, scope, arguments) -> Result<(Value, (AssistantDraftKind, Value)), ToolError>`. Each: parse arguments (private `deny_unknown_fields` structs), check, `user_project(…, ProjectPerm::View)`, validate, and return the normalized draft twice: once in the model's answer `{"accepted": true, "kind": "<kind>", "draft": <draft>}` and once as the `ToolResult.draft`.

   - **`draft_filter`** — arguments: `view` (required: `traces`, `spans`, `sessions`, `users`), `filter` (string, at most 4,096 bytes), `search` (string), `range` (one of `15m`, `1h`, `6h`, `24h`, `7d`, `30d`, the UI's presets) or `from` and `to` (RFC 3339 strings; both or neither; `from < to`; not with `range`), `include_eval` and `include_experiments` (booleans, default false: the list page's `eval` and `exp` switches). Checks: `crate::routes::read::check_search(search)`; the filter with `cetra_storage::validate_filter(&ctx, view, text)` where `ctx = open_query(state, org, project, None)` and the view is `ViewName::Traces` for `traces` and `users` (the users list filters through the traces view), `Spans`, `Sessions`. Errors: `ApiError::QueryAt { field: "filter".to_owned(), error }` for the filter. Draft: `{view, filter?, search?, range?, from?, to?, include_eval, include_experiments}` with blank strings dropped. Kind `filter`.
   - **`draft_sql`** — arguments: `query` (required, not blank, at most 65,536 bytes), `params` (object), `range` or `from`/`to` (as above). Check: open the context with `open_query_windowed(state, org, project, window)` (the window from `from`/`to` or `range` relative to `state.clock.now_ns()`, else `None`), `crate::datasets::sql_views::register(state, &ctx, org, project, &query, window)`, then `ctx.sql_check(&query, &params).await.map_err(ApiError::Sql)?`. Draft: `{query, params, range?|from?/to?, columns: [{name, type}]}`. Kind `sql`.
   - **`draft_dashboard`** — arguments: `dashboard_id` (uuid) with `version` (integer) to edit an existing dashboard, else a new one; `name` (required), `description`, `definition` (required, the `Definition` type of `api/dashboards.rs`, deserialized with serde: a shape error is `bad_request` on `definition`). Checks exactly as `create_dashboard` in `routes/dashboards.rs` calls them (copy its `let mut errors = Vec::new();` and its calls): `checked_name(&name, "name", &mut errors)`, `checked_description(description.as_deref(), "description", &mut errors)`, `check_definition(&definition, "definition", &mut errors)`, `refuse_if_any(errors)?`, `check_expressions(state, &access, &definition, "definition").await?`. For an edit: `dashboard_detail(state, &access, id)` (404 when missing), then a `version` other than the stored one is `ApiError::StaleVersion { current }`. Draft: `{dashboard_id?, version?, name, description?, definition}`. Kind `dashboard`.
   - **`draft_evaluator`** — arguments: `name`, `kind` (`llm_judge` or `check`; `webhook` is refused with `bad_request` "webhook evaluators cannot be drafted", D-AS-6), `config` (object), `output_score` (the `OutputScoreBody` type). Checks: `crate::eval::evaluator::prepare(Uuid::now_v7(), &name, kind, &config, &output_of(&output_score), None, "")` (field errors → `ApiError::BadRequest { detail: None, errors }`), then `check_against_store(state.meta.reader(), org, project, &prepared).await?`. Draft: `{name, kind, config, output_score}` exactly as given (the editor normalizes on save). Kind `evaluator`.
   - **`draft_online_rule`** — arguments: `name`, `evaluator_id` (required uuid), `target` (`trace`, `span`), `filter`, `variable_mapping` (object of strings), `sample_rate` (default 1), `budget_usd`, `budget_period` (`day`, `month`), `enabled` (default false: a draft rule starts disabled). Checks: build a `RuleDraft` (filter through `normalize_filter`), load the evaluator with `crate::meta::evaluators::get_evaluator(state.meta.reader(), project, evaluator_id)`, then `validate(state, org, project, &draft, evaluator.as_ref(), state.clock.now_ns()).await?`. Draft: the `CreateOnlineRuleRequest` fields as JSON. Kind `online_rule`.

3. Descriptions (constants): each says it validates and returns a draft that the user applies; nothing is saved. For example `draft_filter`: "Propose a filter, search and time range for the traces, spans, sessions or users list. It is validated and shown to the user as an Apply button; nothing is saved." Write the other four in the same form, naming what is proposed (a SQL query for the console; a new dashboard or an edit of one, with its full definition; an LLM judge or a check evaluator; an online rule over an existing evaluator, created disabled).

4. Register the five in `specs()` and `run` (last), setting `ToolResult.draft` and `rows: None`.

**Tests** (in `drafts.rs` `mod tests`):
- `draft_filter_validates_each_view`: a valid filter for each of the four views is accepted with the right kind and draft; `nope = 1` on traces is `invalid_filter` on field `filter` with a position.
- `draft_filter_range_rules`: `range` with `from` is `bad_request`; `range: "2h"` is `bad_request`; `from` without `to` is `bad_request`.
- `draft_sql_returns_columns`: a valid query answers its columns; `DELETE FROM traces` is `invalid_filter` on `query`.
- `draft_dashboard_new_and_edit`: a one-widget definition is accepted; an edit with a wrong `version` is `conflict`; a widget with `w: 61` is `bad_request` with the field path `definition.widgets[0].layout.w` (or the exact path `check_definition` gives).
- `draft_evaluator_refuses_webhooks`: `kind: "webhook"` → `bad_request`.
- `draft_evaluator_checks_the_connection`: an `llm_judge` naming a random connection id (no connection exists) → `bad_request` on `config.connection_id`.
- `draft_online_rule_checks_mapping`: over a created check evaluator, a mapping without `output` → `bad_request`; a valid one (`{"output": "trace.output"}` or the mapping `routes/online_rules.rs`'s tests use) is accepted with `enabled: false`.
- `drafts_write_nothing`: after every accepted draft, the dashboards, evaluators and online rules lists of the project are unchanged.
- `every_tool_is_registered`: `specs()` has 24 entries; `run` answers each name with something other than `UnknownTool` (with empty arguments: an `ok: false` is fine).

**Done when:** `just check-rust` passes.

**Commit:** `Add the assistant's draft tools (AS M2)`
