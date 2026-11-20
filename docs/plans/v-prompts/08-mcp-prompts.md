# V · M8 — MCP tools for prompts (design Q2)

The user answered the design's open questions on 2026-11-20: **Q2 yes** (MCP tools for prompts, recorded as D-V-39) and **Q1 later** (no server-side dataset runs; D18 stays; D-V-40). This plan has one phase, **V8.1**, implemented by **Haiku** and reviewed by **Sonnet**. If the plan and the code disagree, **STOP and report**.

**Read first:** `AGENTS.md`; `docs/spec/09-api.md` § MCP server; `crates/server/src/mcp/mod.rs` (`run_tool`, the `match name` dispatch, its tests), `crates/server/src/mcp/tools.rs` (`tool_list`, `tool`, `arguments`, `get_schema` as the model of a project-scoped tool: how it resolves `project` and checks `ProjectPerm::View`), `crates/server/src/mcp/output.rs` (the `max_chars` cap and `notes`), `crates/server/src/meta/prompts.rs` (`list_prompts`, `PromptFilter`, `get_prompt`, `get_version`, `resolve_label`), `crates/server/src/routes/prompts.rs` (`version_json`, `prompt_json`).

## V8.1 — `list_prompts` and `get_prompt`

**Commit subject:** `mcp: add the list_prompts and get_prompt tools`

### Edit

| File | Change |
|---|---|
| `crates/server/src/mcp/prompts.rs` | new: the two tools |
| `crates/server/src/mcp/mod.rs` | `mod prompts;`, two dispatch arms, tests |
| `crates/server/src/mcp/tools.rs` | two `tool(...)` entries in `tool_list`, after `list_saved_views` |
| `crates/server/src/routes/prompts.rs` | `version_json` and `prompt_json` become `pub(crate)` (visibility only) |
| `docs/spec/09-api.md` | two rows in the MCP tools table; "7 read-only tools" wording if present |
| `website/src/content/docs/docs/platform/mcp.mdx` | the two tools in its tool list |
| `docs/mvp-decisions.md`, `docs/plans/v-prompts/00-design.md` | D-V-39, D-V-40; the design's § Open questions marked answered; D-H-11's tool count, if it states one, gets "(nine since D-V-39)" |
| `CHANGELOG.md` | one `### Added` line |

### Tools

```text
list_prompts  { project, q?, label?, limit? (1–200, default 50), max_chars? }
  → { prompts: [{ name, type, description?, tags, latest_version, labels: { <label>: <version> } }], truncated, notes }
  Prompts of the project, ordered by name (meta::prompts::list_prompts with PromptFilter { q, label (None for "latest"), tag: None }), the first `limit`; `truncated` when more exist.

get_prompt    { project, name, label?, version?, max_chars? }
  → the version as GET /prompts/{name}/resolve answers it (version_json: name, type, version, template, config, labels, variables, placeholders, commit_message?, created_by?, created_at)
  `label` and `version` together → invalid params ("give label or version, not both"); default label `production`; `latest` → the latest version; an unknown prompt, label or version → the tool's not-found error, as get_trace answers a missing trace.
```

Both: `project` resolved and `ProjectPerm::View` checked exactly as `get_schema` does; the result is compact JSON cut to `max_chars` (default 20 000, 1 000–100 000) through `output.rs` as the other tools do, with a note when cut. Descriptions (tool list): `list_prompts` — "Prompts of the project's registry, with their latest version and labels (production, staging, …)."; `get_prompt` — "One prompt version: its template ({{ name }} variables, chat messages and placeholders), config (model, params, tools) and labels. Defaults to the version labelled production; pass `label` or `version` for another."

No logs with prompt names or template text (AGENTS.md § Privacy).

### Tests (`mcp/mod.rs`, in the style of the existing tool tests)

1. `tools/list` includes `list_prompts` and `get_prompt` (and the existing list assertions are updated).
2. `list_prompts` on a project with two prompts returns both, ordered; `q` filters; `limit: 1` sets `truncated`.
3. `get_prompt` returns the production version by default, `version: 2` and `label: "latest"` the latest; `label` + `version` → invalid params; an unknown name → not found.
4. A PAT of a user without access to the project → the same error the other tools give.

### Done when

`export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` passes (one run at the end; the user's rule).

### Decisions (text for `docs/mvp-decisions.md` § Track V and the design's tables)

- **D-V-39 — MCP prompt tools (user, 2026-11-20: Q2 yes).** The MCP server gains read-only `list_prompts` and `get_prompt` (the resolve fetch's answer), with the same auth, limits and `max_chars` cap as the other tools.
- **D-V-40 — No server-side dataset runs yet (user, 2026-11-20: Q1 later).** The playground does not run over dataset items and there is no server-side prompt experiment; D18 stays. Phoenix and Opik run prompts over datasets from the playground and Langfuse has prompt experiments: to revisit after launch.

## Precisions (from the cold review; they override the text above)

1. **Tool list test.** `crates/server/src/mcp/mod.rs` `const TOOLS: [&str; 7]` becomes `[&str; 9]` with `"list_prompts"`, `"get_prompt"` right after `"list_saved_views"`; the two `tools/list` tests that assert it (2025 and 2026 protocol versions) then pass unchanged. `INSTRUCTIONS` says "Read-only access to LLM traces, spans and prompts".
2. **Errors.** `label` + `version` together → `ApiError::BadRequest { detail: Some("give label or version, not both".into()), errors: vec![] }` (an `isError` result with code `bad_request`). Missing prompt, label or version → `ApiError::NotFound` (code `not_found`), as `get_trace` does. Tests assert `is_error == true` and those codes.
3. **`get_prompt` resolution.** Parse args with a local `#[derive(Deserialize)] #[serde(deny_unknown_fields)]` struct through `tools::arguments`; `access = user_project(state, user, &args.project, ProjectPerm::View).await?` (as `get_schema`); `row = meta::prompts::get_prompt(state.meta.reader(), access.project.id, &args.name).await?.ok_or(ApiError::NotFound)?`; then `version` → `get_version(reader, row.id, v)`, label `latest` → `get_version(reader, row.id, row.latest_version)`, any other label (default `production`) → `resolve_label(reader, row.id, label)`; `None` → `NotFound`. Answer `serde_json::to_value(routes::prompts::version_json(&row, &version)?)` (map a serde error to `ApiError::Internal(Box::new(e))`). Make only `version_json` `pub(crate)` (not `prompt_json`). `get_prompt` takes no `max_chars`: the whole version is returned (as `get_schema`); remove it from its schema.
4. **`list_prompts` items** are built from `PromptRow`: `{name, type: p.prompt_type.as_str(), description (omitted when none), tags, latest_version, labels: {label: version}}` from `p.labels` (`latest` is not added). `limit` 1–200 checked locally (`ApiError::BadRequest` "limit must be 1 to 200"); `max_chars` is `Option<i64>` through `output::max_chars`. Body: `{prompts, truncated, notes}`; when more than `limit` prompts exist set `truncated = true` and push the note "more prompts than limit; narrow with q or label"; then `output::fit_list(&mut body, "prompts", max, "result cut at N characters; later prompts left out")` (follow its real signature), which appends to `notes`.
5. **Docs.** `docs/spec/09-api.md` MCP tools table: two rows. `website/src/content/docs/docs/platform/mcp.mdx`: two table rows and "seven read-only tools" → "nine read-only tools". `docs/mvp-decisions.md` D-H-11: "Seven read-only tools" gets "(nine since D-V-39)". Historical plans and the roadmap's track H row stay as they are.
