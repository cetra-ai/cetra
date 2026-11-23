# Plan: the LLM connection form, draft tests and the stored last test

The org settings page LLM connections (`/$org/settings/llm-connections`) opens a panel with `ConnectionForm`, a danger zone and a Test dialog. This plan reworks that panel. **Polish:** the fields are capped at a readable width, the order is Provider, Name, API key, Base URL, Default model, Advanced; a saved key shows as `••••<hint>`; the Base URL placeholder is the provider's default URL; hints are short and name no server setting; Structured output moves into Advanced. **Provider-aware form (web only):** four choices, OpenAI, OpenAI-compatible, Anthropic and Gemini, over the unchanged `openai | anthropic | gemini` adapter; one `providerLabel` everywhere a connection is shown; Default model suggestions from a static list of model ids. **Test before saving:** the Test button tests the form's current values through a draft test, saved or not, and shows the result under the form; the Test dialog goes away; the embedding test moves into Advanced. **Last test stored:** `meta.db` keeps the time and outcome of the last test of the saved values, the API returns them, and the panel header shows "Tested 5m ago" or "Last test failed 5m ago".

**Prerequisites:** branch `claude/connection-form`, cut from `origin/main` at `aecc26b` ("Mark track EA's design as approved"). Worktree `/home/sebastien/travaux/llm-observer/.claude/worktrees/connection-form`. Decisions D-I-51 to D-I-54 are already in `docs/mvp-decisions.md` (track I section), with the user's answers to Q1–Q3, and D-I-4's help-text sentence already points to D-I-51.

## The user's answers to the open questions (2026-11-23)

**Q1. The saved key and a new base URL: drop the guard.** Until now `update_llm_connection` refused a body with `base_url` or `adapter` and no `api_key` (`KEY_NEEDED_FOR_HOST`, `crates/server/src/routes/connections.rs:44` and `:250`, test `patch_of_the_host_needs_the_key_again`, spec 09 line 72). That rule goes (S2): a PATCH of `base_url` or `adapter` without `api_key` keeps the saved key, and a draft test without `api_key` uses the saved key, whatever its `base_url` or `adapter`. Choosing OpenAI on an edit just sends `base_url: ""`. Recorded in D-I-53, and D-I-51 points to it.

**Q2. Default model suggestions: a static list in the web, no server change.** `GET /api/v1/orgs/{org}/models` holds only the org's own rows, so it is not used. `web/src/lib/llm-models.ts` (W2) holds a short list of model ids per provider, every id taken from the built-in price table (`crates/otel/prices/builtin.json`, with that provider). OpenAI-compatible gets none. Recorded in D-I-52.

**Q3. The assistant panel's label: `AssistantStatus` gains `base_url`** (when usable and set), S3. Every reader of that route is a project member, hence an org member who can already list the connections with their base URLs (spec 08 line 96).

**Choices this plan makes where the brief left room** (recorded in D-I-51 to D-I-54):
1. A chat test of the saved values records the last test only on the connection's default model; a typed model records nothing (D-I-5 already treats a typed model as saying nothing about the connection). An embedding test of the saved values always records (it has no default model). An Anthropic embedding refusal makes no call and records nothing.
2. `api_key` and `extra_headers` in a draft body always make the test a draft: write-only values are never compared with the saved ones.
3. The form sends `model` only when its default model differs from the saved one, so a saved-values test from the form still counts as a default-model test (resumes pauses, records the last test).
4. A base URL typed in Advanced for the OpenAI choice is saved, and the connection then shows as OpenAI-compatible (D-I-51 says so).
5. The provider select has no hint; the list of compatible servers lives in the OpenAI-compatible Base URL hint only.
6. Invalid draft bodies are refused (400) before the rate limiter, so they do not spend a test.
7. Migration number `0031`. If another branch lands a `0031` first, the coordinator renumbers this one at merge (and its `ALTERING_CASES` entry).

## Read first (every phase)

- `AGENTS.md` (§ Rules, § Tests, § Commits and reports).
- `docs/mvp-decisions.md` § Track I, rows D-I-4, D-I-5, D-I-24 and D-I-51 to D-I-54.
- `docs/spec/09-api.md` line 72 (the connections row) and § Assistant.
- Server phases: `docs/spec/04-metadata-db.md` § Keys & credentials (the `llm_connections` table) and § Format versions; `crates/server/src/routes/connections.rs` (all of it); `crates/server/src/api/connections.rs`; `crates/server/src/meta/connections.rs`; `crates/server/src/meta/migration_tests.rs`.
- Web phases: `docs/spec/11-ui.md` § Settings (lines 239–246); `web/src/components/settings/connection-form.tsx`; `web/src/components/settings/connection-test.tsx`; `web/src/routes/$org/settings/llm-connections.tsx` and its test; `web/src/lib/llm-adapters.ts`.
- Your phase's own section, and the files it names.

## Conventions (every phase)

- **Never guess.** If a step is unclear, contradicts the code, or fails in a way the phase does not explain, STOP. Make no further edits. Your final message starts with `QUESTION:` followed by the question and what you found (file, line, error text). You will be resumed with an answer.
- Work only in the worktree your brief names. Every path in this plan is relative to it. Do not create branches or worktrees. Do not stash, reset, checkout or switch branches. Commit once, at the end, with the phase's subject. No attribution lines.
- Edit only the files your phase lists under **Edit**. If you need a change elsewhere, STOP and ask.
- Cargo: `export CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target`. `cargo nextest` is not installed: use `cargo test`.
- Node: before any `node`, `npx`, `pnpm` or `just` recipe that runs JS: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`. If `node_modules/` is missing in the worktree, run `just js-install` once.
- **Checks (the user's rule):** while several agents run, nobody runs full suites. A phase runs the formatters and only the tests its **Done when** names. Delete `web/.vitest/` if a run creates it. The coordinator runs `just check` once at the end of the plan.
- AGENTS.md rules: no `unwrap`/`expect` outside tests unless a comment states the invariant; no placeholders; no compatibility shims (when a signature changes, update every caller); errors through `ApiError`/`thiserror`; **no API key, header value, base URL, prompt or payload in logs or error messages** (log ids, sizes and fixed reasons); tests for every behaviour.
- Text sizes use the named scale only (`text-caption`, `text-body`, `text-ui`, `text-title`, `text-heading`; D-C-11).
- Web tests use the MSW mocks from `fixtures/api/`; a test that needs another answer overrides the endpoint with `server.use(...)`. Never mock modules.
- Final report: files changed, tests added, the exact commands you ran and their results, any deviation and why, anything not done.

## What it is, and what it is not

When this plan is done:
- **Server.** `llm_connections` has `last_tested_at` and `last_test_ok` (migration `0031`); `LlmConnection` returns them; a saved-values test records them; a PATCH that changes what a test calls clears them. `POST …/llm-connections/{id}/test` takes draft fields; `POST …/llm-connections/test` tests an unsaved connection; a draft stores nothing. A PATCH of `base_url` or `adapter` no longer needs `api_key` (Q1). `GET /projects/{project}/assistant` carries `base_url` (Q3).
- **Web.** The form as D-I-51 describes it; the Default model datalist from `web/src/lib/llm-models.ts` (D-I-52); the Test button in the form footer and "Test embeddings" in Advanced, both testing the current values, with the result under the form (D-I-53); "Tested …" / "Last test failed …" in the panel header (D-I-54); `providerLabel` in the list, panel subtitle, read-only details and assistant panel; the Test dialog and `LLM_ADAPTER_LABELS` removed.
- **Docs.** Specs 04, 06, 09, 11, 12, the LLM connections docs page, CHANGELOG `[Unreleased]`, the track I roadmap row.

Out of scope:
- The adapter enum, the connection columns other than the two new ones, the assistant settings page's own tool-calling test (`POST /orgs/{org}/assistant/test`: it records no last test).
- A "last tested" column in the list (user's choice).
- The playground and the evaluator form's connection pickers (they show connection names only).
- Docs screenshots: they wait for the product freeze (user's rule).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-I-4 | Connections are org-wide; keys and header values write-only; `api_key_hint`; base URL query rule; the UI cannot read `security.allow_private_network_egress`. |
| D-I-5 | Connection test: one boolean judge call; a success sets `ok` and, on the default model, resumes `connection` pauses; a pause-class error on the default model sets `failing`; per-user limit. |
| D-I-24 | Reads need `OrgPerm::View`; writes and tests need `OrgPerm::ManageConnections`. |
| D-IC-5 | The embedding test (`embedding_model`), refused for Anthropic; a success resumes the issue sources paused for the connection. |
| D-I-51 (new) | Form layout, the four provider choices and their mapping, placeholders and hints, `providerLabel`, `AssistantStatus.base_url`. |
| D-I-52 (new) | Default model suggestions: a static list per provider in the web, none for OpenAI-compatible; datalist. |
| D-I-53 (new) | Draft tests: the draft fields, the unsaved route, what a draft is, what it never stores; the form's Test; PATCH and draft tests keep the saved key on a new `base_url` or `adapter`. |
| D-I-54 (new) | `last_tested_at` / `last_test_ok`: when they are recorded and cleared; the header line. |

The user's answers (2026-11-23, through the coordinator): every item A1–A5, B6–B8 and C9–C13 of the brief, as recorded in D-I-51 to D-I-54. The UI strings fixed by the user: "Enter the server's base URL.", "Any server with an OpenAI-style API: Azure OpenAI, vLLM, Ollama, OpenRouter, LiteLLM.", "Stored encrypted. Type a new key to replace it.", "Enter a default model to test.", "Anthropic has no embeddings API", "Connection works (N ms, model M)" (the existing `ConnectionTestResult`), "Tested <relative time>", "Last test failed <relative time>".

## Versions

No new dependency (Rust or JS).

## Phases and dependencies

```
S1 last test stored ──> S2 draft tests, PATCH key rule ──> S3 assistant base_url ──> W1 provider labels ──> W2 provider-aware form ──> W3 test from the form, header
D1 specs 11/12, docs page, changelog, roadmap  (parallel with everything)
```

| Phase | Edit (summary) | Runs |
|---|---|---|
| S1 | migration `0031`, `meta/{connections,migration_tests}.rs`, `api/connections.rs`, `routes/connections.rs`, `eval/{connection,interactive}.rs`, `golden_tests/dump.rs`, two static fixtures, spec 04 and 09; generated `openapi.json`, `schema.gen.ts` | first |
| S2 | `api/connections.rs`, `routes/connections.rs`, `api/openapi.rs`, `web/src/mocks/writes.ts`, spec 06 and 09; generated `openapi.json`, `schema.gen.ts` | after S1 |
| S3 | `api/assistant.rs`, `routes/assistant.rs`, spec 09; generated `openapi.json`, `schema.gen.ts` | after S2 |
| W1 | `web/src/lib/llm-adapters.ts` (+ new test), `llm-connections.tsx` (+ test), `assistant-panel.tsx` (+ test) | after S3 |
| W2 | `connection-form.tsx`, `form/text-field.tsx`, `lib/llm-adapters.ts` (+ test), `lib/llm-models.ts` (new, + test), `llm-connections.test.tsx` | after W1 |
| W3 | `connection-form.tsx`, `connection-test.tsx`, `llm-connections.tsx`, `llm-connections.test.tsx` | after W2 |
| D1 | `docs/spec/11-ui.md`, `docs/spec/12-operations.md`, `website/src/content/docs/docs/evaluation/llm-connections.mdx`, `CHANGELOG.md`, `docs/roadmap.md` | parallel with all |

The server phases are sequential because each regenerates `crates/server/openapi.json` and `web/src/api/schema.gen.ts` ; no other phase touches a generated file. The web phases are sequential because they share `connection-form.tsx` and `llm-connections.test.tsx`, and W1 needs S3's `AssistantStatus.base_url` in `schema.gen.ts`. D1 runs in its own worktree from the same base and is merged at the end. A Sonnet code review runs after S3 and after W3; then the coordinator runs `just check` and looks at the panel in a browser on the mocks (`just web-dev`).

---

## S1: store the last connection test

**Goal:** add `last_tested_at` / `last_test_ok` to `llm_connections` and `LlmConnection`, record them on a saved-values test, and clear them on a PATCH that changes what a test calls.

**Edit:**
- `crates/server/migrations/meta/0031_connection_last_test.sql` (new)
- `crates/server/src/meta/migration_tests.rs`
- `crates/server/src/meta/connections.rs`
- `crates/server/src/api/connections.rs`
- `crates/server/src/routes/connections.rs`
- `crates/server/src/eval/connection.rs` (test literal at line 143 only)
- `crates/server/src/eval/interactive.rs` (test literal at line 78 only)
- `crates/server/tests/snapshots/meta_schema.snap` (rewritten by step 15)
- `crates/server/src/golden_tests/dump.rs`
- `fixtures/api/static/llm-connection.json`, `fixtures/api/static/llm-connections.json`
- `docs/spec/04-metadata-db.md`, `docs/spec/09-api.md`
- generated, by step 14 only: `crates/server/openapi.json`, `web/src/api/schema.gen.ts`

**Steps:**

1. Create `crates/server/migrations/meta/0031_connection_last_test.sql` exactly:

```sql
-- The last test of a connection's saved values (D-I-54).
ALTER TABLE llm_connections ADD COLUMN last_tested_at INTEGER;
ALTER TABLE llm_connections ADD COLUMN last_test_ok INTEGER;
```

2. In `crates/server/src/meta/migration_tests.rs`, append this entry at the end of `ALTERING_CASES` (after the `version: 30` entry). Do not touch `PRE_RELEASE_NON_ADDITIVE` or `FORMAT_CHANGING_MIGRATIONS` (the migration is additive; `META_FORMAT_VERSION` stays 1):

```rust
    AlteringCase {
        version: 31,
        seed: &[
            "INSERT INTO orgs (id, name, slug, state, created_at, updated_at)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000001c1', 'Acme', 'acme', 'active', 1, 2)",
            "INSERT INTO llm_connections (id, org_id, name, adapter, secret_enc, api_key_hint,
            created_at, updated_at)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000001c2', '018f6b4e-7c1a-7c1a-8c1a-0000000001c1',
            'kept', 'openai', x'01', 'WXYZ', 3, 4)",
        ],
        read: "SELECT id || '|' || name || '|' || adapter || '|' || api_key_hint || '|' || status
            || '|' || created_at || '|' || updated_at || '|' || COALESCE(last_tested_at, 'NULL')
            || '|' || COALESCE(last_test_ok, 'NULL') FROM llm_connections",
        expected: "018f6b4e-7c1a-7c1a-8c1a-0000000001c2|kept|openai|WXYZ|ok|3|4|NULL|NULL",
    },
```

3. In `crates/server/src/meta/connections.rs`:
   1. `ConnectionRow`: after `last_error_at`, add
      ```rust
      /// When the saved values were last tested (D-I-54).
      pub(crate) last_tested_at: Option<i64>,
      /// Whether that test succeeded; `None` with `last_tested_at`.
      pub(crate) last_test_ok: Option<bool>,
      ```
   2. The hand-written `Debug`: add `.field("last_tested_at", &self.last_tested_at)` and `.field("last_test_ok", &self.last_test_ok)` after `last_error_at`.
   3. `ConnectionRecord`: after `last_error_at: Option<i64>,` add `last_tested_at: Option<i64>,` and `last_test_ok: Option<bool>,` (sqlx decodes SQLite `INTEGER` 0/1 as `bool`, as `meta/alerts.rs` does). `row_from`: copy both.
   4. The three SELECTs only — in `list_connections` (line 129), `get_connection` (line 152) and `get_connection_tx` (line 176); **not** the INSERT column list of `insert_connection` (line 219), which has the same text. In each SELECT, the two lines
      ```
               requests_per_minute, status, last_error, last_error_at, created_by, created_at, \
               updated_at \
      ```
      become exactly
      ```
               requests_per_minute, status, last_error, last_error_at, last_tested_at, last_test_ok, \
               created_by, created_at, updated_at \
      ```
      (keep the file's own indentation). `insert_connection` and `update_connection` do not change (a new row gets NULL; `update_connection` leaves the two columns alone). `row_from` (line 366) is the only non-test `ConnectionRow {` literal of the file; the test literal is `expected` in `insert_list_get_round_trip` (line 520).
   5. After `set_connection_status`, add exactly:

```rust
/// Stores the outcome of a test of the saved values (D-I-54), without touching `updated_at`,
/// only while the row's `updated_at` is still `tested_updated_at`: a change saved during the
/// test call makes the outcome stale, and it is dropped. Returns whether the row was updated.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn record_connection_test(
    conn: &mut SqliteConnection,
    id: Uuid,
    tested_updated_at: i64,
    ok: bool,
    now: i64,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "UPDATE llm_connections SET last_tested_at = ?, last_test_ok = ? \
         WHERE id = ? AND updated_at = ?",
    )
    .bind(now)
    .bind(ok)
    .bind(id.to_string())
    .bind(tested_updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Forgets the stored test (D-I-54), without touching `updated_at`.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn clear_connection_test(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE llm_connections SET last_tested_at = NULL, last_test_ok = NULL WHERE id = ?",
    )
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}
```

   6. Tests module: add `last_tested_at: None, last_test_ok: None` to the `expected` `ConnectionRow` literal of `insert_list_get_round_trip`; import the two new functions; add the tests listed below.

4. In `crates/server/src/api/connections.rs`, `LlmConnection`: after `last_error_at`, add exactly:

```rust
    /// When the saved values were last tested on the default model or an embedding model.
    /// Omitted when never tested, and after a change of the adapter, key, base URL, structured
    /// output or headers. JSON is RFC 3339 (D-I-54).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_tested_at: Option<i64>,
    /// Whether that test succeeded. Omitted with `last_tested_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test_ok: Option<bool>,
```

5. In `crates/server/src/routes/connections.rs`, `connection_json`: set `last_tested_at: row.last_tested_at, last_test_ok: row.last_test_ok` (after `last_error_at`).

6. Same file, `update_llm_connection`:
   1. Next to `let key_sent = key.is_some();` add `let headers_sent = headers.is_some();`.
   2. Inside the write closure, after `let fields = ConnectionFields { … };` and **before** `let Some(row) = connections::update_connection(…)`, add:

```rust
                // The stored test no longer describes what a test would call (D-I-54).
                let clears_test = key_sent
                    || headers_sent
                    || fields.adapter != row.adapter
                    || fields.base_url != row.base_url
                    || fields.structured_output != row.structured_output;
                if clears_test {
                    connections::clear_connection_test(conn, id).await?;
                }
```

   (Clearing before `update_connection` makes the row it reads back carry the cleared values.)

7. Same file, add after `set_status`:

```rust
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
```

8. Same file, `test_connection`: after the `if let Err(error) = status_write { … }` block and before the audit `tracing::info!`, add:

```rust
    // Only a test of the default model describes the connection (D-I-54).
    if explicit.is_none() {
        record_test(&state, id, row.updated_at, result.ok, now).await;
    }
```

   `test_embedding`: after the `if result.ok { … }` block and before its audit `tracing::info!`, add `record_test(state, id, row.updated_at, result.ok, now).await;`. The Anthropic early return stays before any write and records nothing.

9. Add `last_tested_at: None,` and `last_test_ok: None,` after `last_error_at: None,` in the `ConnectionRow` literals of `crates/server/src/eval/connection.rs:143` (test fn `row`) and `crates/server/src/eval/interactive.rs:78` (test fn `row`); `meta/connections.rs:366` (`row_from`) and `:520` (test) are handled in step 3. Then run `rtk proxy grep -rn "ConnectionRow {" crates/server/src` and `rtk proxy grep -rn "LlmConnection {" crates/server/src`; every literal must have the two fields. Any other literal: STOP and ask.

10. `crates/server/src/golden_tests/dump.rs`, `connections_section`: after `"last_error": row.last_error,` add `"last_tested_at": row.last_tested_at,` and `"last_test_ok": row.last_test_ok,`.

11. Fixtures: in `fixtures/api/static/llm-connection.json` and in the one item of `fixtures/api/static/llm-connections.json`, insert after the `"status": "ok",` line (same indentation as its siblings):
    ```json
      "last_tested_at": "2026-01-01T09:05:00.000000000Z",
      "last_test_ok": true,
    ```
    (the key order is the struct order, which the canonical-JSON round-trip checks).

12. `docs/spec/04-metadata-db.md`, `CREATE TABLE llm_connections`: after the `last_error_at INTEGER,` line add
    ```
      last_tested_at INTEGER,             -- last test of the saved values (D-I-54)
      last_test_ok INTEGER,               -- 1 ok, 0 failed; NULL with last_tested_at
    ```
    and append to the paragraph after the block ("Connections belong to the org (D-I-4). …") the sentence: "`last_tested_at` and `last_test_ok` hold the last test of the saved values; a change of what a test calls clears them (D-I-54)."

13. `docs/spec/09-api.md` line 72: replace "responses carry `api_key_hint` and `extra_header_names`;" with "responses carry `api_key_hint` and `extra_header_names`, and `last_tested_at` and `last_test_ok` once the saved values were tested;" and replace the row's closing "(D-I-4, D-I-5) |" with "; a PATCH that changes `adapter`, `base_url` or `structured_output`, or sends `api_key` or `extra_headers`, clears the last test (D-I-4, D-I-5, D-I-54) |". Do not touch the row's other clauses (S2 rewrites the whole row, including the PATCH key rule).

14. Regenerate: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just openapi`.

15. Rewrite the meta.db schema snapshot (`cargo insta` is not installed): `INSTA_UPDATE=always cargo test -p cetra-server --all-features --lib meta::tests::schema_matches_the_snapshot`. Then `git diff crates/server/tests/snapshots/meta_schema.snap` must show only the `llm_connections` table's SQL gaining `last_tested_at INTEGER` and `last_test_ok INTEGER`; anything else: STOP and ask. Delete any `*.snap.new` file left next to it.

**Tests** (all in the existing `#[cfg(test)] mod tests` of the file named):
- `meta/migration_tests.rs`: the new case runs inside `migrations_that_alter_a_table_keep_its_rows`; `every_altering_migration_has_a_case` and `migrations_only_add` must pass unchanged.
- `meta/connections.rs`:
  - `record_connection_test_needs_the_tested_updated_at`: insert at `NOW`; `record_connection_test(conn, id, NOW + 1, true, NOW + 5)` returns `false` and the row's two fields stay `None`; `record_connection_test(conn, id, NOW, false, NOW + 5)` returns `true`, then `get_connection` has `last_tested_at == Some(NOW + 5)`, `last_test_ok == Some(false)`, `updated_at == NOW`.
  - `clear_connection_test_clears_both`: record, then clear; both `None`, `updated_at` unchanged.
- `routes/connections.rs` (use the existing helpers `egress_app`, `mocked`, `test`, `patch`, `get`, `create`, `text`):
  - `a_new_connection_has_no_last_test`: create; `get(...).json::<Value>()` has neither key `last_tested_at` nor `last_test_ok`.
  - `a_saved_test_records_the_last_test`: `mocked`, `test(…, json!({}))` ok; `get` → `last_tested_at.is_some()`, `last_test_ok == Some(true)`.
  - `a_failed_saved_test_records_a_failed_last_test`: mock replies `Reply { status: 401, … }` as in `a_401_marks_the_connection_failing`; `last_test_ok == Some(false)`.
  - `a_test_on_a_typed_model_records_nothing`: `json!({"model": "other-model"})` ok; both `None`.
  - `an_embedding_test_records_the_last_test`: `MockLlm::start_with_embeddings(Vec::new(), vec![embedding(&[0.6, 0.8])])`, body `{"embedding_model": "text-embedding-test"}`; `last_test_ok == Some(true)`.
  - `an_anthropic_embedding_refusal_records_nothing`: as `an_embedding_test_on_anthropic_is_refused_without_a_call`; both `None`.
  - `a_patch_of_the_key_clears_the_last_test`: test ok, then `patch {"api_key": "sk-second-key-0123456789"}`; the PATCH response and `get` have both `None`.
  - `a_patch_of_the_structured_output_clears_the_last_test`: `{"structured_output": "tool"}` → `None`.
  - `a_patch_of_the_name_or_the_same_mode_keeps_the_last_test`: `{"name": "Renamed"}` then `{"structured_output": "native"}` → `last_test_ok == Some(true)` after each.
  - Existing `a_failed_status_write_keeps_the_test_result` must still pass: its trigger also blocks `record_test`, which logs the warning "could not store the connection test"; that warning is expected, the test does not assert on logs.

**Done when:**
```
export CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target
cargo fmt -p cetra-server
cargo clippy -p cetra-server --all-targets --all-features -- -D warnings
cargo test -p cetra-server --all-features --lib -- meta::connections:: meta::migration_tests:: meta::tests::schema_matches_the_snapshot routes::connections:: eval::connection:: eval::interactive:: golden_tests:: api_examples::
cargo test -p cetra-server --all-features --test openapi --test api_examples
```
`git status` shows only the **Edit** files (the snapshot and the two generated ones included).

**Commit:** `Store the last test of an LLM connection`

---

## S2: draft connection tests

**Goal:** let `POST …/llm-connections/{id}/test` take draft fields and add `POST …/llm-connections/test` for an unsaved connection; a draft stores nothing; a PATCH of `base_url` or `adapter` no longer needs `api_key` (Q1).

**Edit:**
- `crates/server/src/api/connections.rs`
- `crates/server/src/routes/connections.rs`
- `crates/server/src/api/openapi.rs`
- `crates/server/tests/openapi.rs`
- `web/src/mocks/writes.ts`
- `docs/spec/06-evaluation.md`, `docs/spec/09-api.md`
- generated, by step 10 only: `crates/server/openapi.json`, `web/src/api/schema.gen.ts`

**Steps:**

1. In `crates/server/src/api/connections.rs`, replace `TestLlmConnectionRequest` and add `TestUnsavedLlmConnectionRequest` after it, exactly:

```rust
/// `POST /api/v1/orgs/{org}/llm-connections/{id}/test` body. The draft fields (`adapter`,
/// `api_key`, `base_url`, `structured_output`, `extra_headers`) have PATCH semantics: absent means
/// the saved value, including the saved key, which a draft `base_url` or `adapter` also uses
/// when `api_key` is absent. A test with a draft field that differs from the saved
/// value, or with `api_key` or `extra_headers`, stores nothing (D-I-53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestLlmConnectionRequest {
    /// Model to call. Default: the connection's `default_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Embedding model to test instead: the test embeds `"ping"` and does not ask for a
    /// judgement. Not for an Anthropic connection, which has no embeddings API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// Draft provider API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<LlmAdapter>,
    /// Draft API key. Write-only. Absent: the saved key, whatever the draft `base_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Draft provider endpoint. An empty string means the provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Draft structured output mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<StructuredOutputMode>,
    /// Draft extra headers, replacing the saved ones. Values are write-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_headers: Option<BTreeMap<String, String>>,
}

/// `POST /api/v1/orgs/{org}/llm-connections/test` body: a connection that is not saved. Nothing
/// is stored (D-I-53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestUnsavedLlmConnectionRequest {
    /// The provider API.
    pub adapter: LlmAdapter,
    /// Provider API key, 1 to 4096 bytes. Write-only.
    pub api_key: String,
    /// Provider endpoint overriding the provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// How the provider is asked to fill the answer schema. Default `native`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<StructuredOutputMode>,
    /// Extra headers sent with the call, at most 20. Values are write-only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_headers: BTreeMap<String, String>,
    /// Model to call. Required unless `embedding_model` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Embedding model to test instead. Not for Anthropic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
}
```

2. In `crates/server/src/routes/connections.rs`:
   1. Imports: add `use std::sync::Arc;`, `TestUnsavedLlmConnectionRequest` to the `crate::api::connections` import.
   2. Constants: delete `KEY_NEEDED_FOR_HOST` (Q1) and add
      ```rust
      /// An unsaved connection has no default model (D-I-53).
      const UNSAVED_MODEL_MESSAGE: &str = "model is required to test an unsaved connection";
      /// The refusal text of an embedding test on Anthropic (D-IC-5).
      const NO_EMBEDDINGS_MESSAGE: &str = "This provider has no embeddings API.";
      ```
      and use `NO_EMBEDDINGS_MESSAGE` in place of the literal in the existing refusal.
   3. `routes()`: add after the `…/{id}/test` entry:
      ```rust
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
      ```

3. Same file, add the draft type and its helpers (above `test_connection`):

```rust
/// The draft fields of a connection test (D-I-53), with PATCH semantics.
struct DraftFields {
    adapter: Option<LlmAdapter>,
    api_key: Option<String>,
    base_url: Option<String>,
    structured_output: Option<StructuredOutputMode>,
    extra_headers: Option<BTreeMap<String, String>>,
}
```

   `impl DraftFields` with three methods, bodies as follows:
   - `fn check(&self, errors: &mut Vec<FieldError>)`: in this order: `check_api_key` when `api_key` is `Some`; `check_base_url(self.base_url.as_deref(), errors)`; `check_extra_headers` when `extra_headers` is `Some`. No rule ties `base_url` or `adapter` to `api_key` (Q1): an absent key is the saved key.
   - `fn differs_from(&self, row: &ConnectionRow) -> bool`: `true` when `api_key.is_some()`, or `extra_headers.is_some()`, or `adapter` is `Some(a)` with `a.as_str() != row.adapter`, or `base_url` is `Some(b)` with `empty_to_none(b.trim()) != row.base_url`, or `structured_output` is `Some(m)` with `m.as_str() != row.structured_output`. Else `false`.
   - `fn apply(&self, mut base: cetra_eval::JudgeConnection) -> Result<cetra_eval::JudgeConnection, ApiError>`: for each field that is `Some`, replace the matching field of `base`: `adapter` → `base.adapter = eval_adapter(a)?`; `api_key` → `base.api_key = cetra_eval::Secret::new(key.clone())`; `base_url` → `base.base_url = parsed_base_url(b)?`; `structured_output` → `base.structured_output = eval_structured(m)?`; `extra_headers` → `base.extra_headers = header_secrets(h)`. Return `Ok(base)`.

   Free functions (exact):

```rust
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
```

   (`cetra_eval::UnknownName` implements `std::error::Error`, `crates/eval/src/judge/mod.rs:122`, so `internal` accepts it.)

4. Same file, `update_llm_connection` (Q1): delete the check and its comment
   ```rust
       // A new host must not receive the saved key, as a webhook's new URL needs its secret.
       if (body.base_url.is_some() || body.adapter.is_some()) && body.api_key.is_none() {
           errors.push(field_error("api_key", KEY_NEEDED_FOR_HOST));
       }
   ```
   In `crates/server/src/api/connections.rs`, `UpdateLlmConnectionRequest`: the doc of `adapter` becomes `/// The provider API the connection talks to. Without \`api_key\`, the saved key is kept.` and the doc of `base_url` becomes `/// Provider endpoint. An empty string clears it. Without \`api_key\`, the saved key is kept and sent to the new endpoint.` (they flow into `openapi.json`). Nothing else in the handler changes: an absent `api_key` already keeps `row.secret_enc` and `row.api_key_hint`, and `state.connections.forget(id)` already drops the cached connection, so the next call uses the new host with the saved key. S1's `clears_test` still clears the last test on a new `base_url` or `adapter`.

5. Same file, split the provider calls out of the handlers, moving code without changing it:
   - `async fn run_chat_test(state: &AppState, org_id: OrgId, connection: &cetra_eval::JudgeConnection, evaluator_id: Uuid, model: String) -> Result<(LlmConnectionTestResult, bool), ApiError>`: the body of today's `test_connection` from `let evaluator = cetra_eval::Evaluator {` through the `if let Err(error) = run.outcome { … }` block, with `row.id` replaced by `evaluator_id` (both in `id` and `connection_id`), `access.org.id` by `org_id`, and `connection: Some(connection)` in `RunInput`. Returns `(result, pause_class)`.
   - `async fn run_embedding_test(state: &AppState, org_id: OrgId, connection: &cetra_eval::JudgeConnection, model: String) -> Result<LlmConnectionTestResult, ApiError>`: the body of today's `test_embedding` from `let ingest = cetra_otel::IngestContext {` through its `if let Err(error) = run.outcome { … }` block, with `connection` passed as `connection` (not `connection.as_ref()`). Returns `result`.
   - Delete `test_embedding`; its status write moves into `test_connection` (step 6).

6. Rewrite `test_connection` in this order:
   1. `refuse_outbound(&state, "connection test")?;` then `let user = principal::session_user(&principal)?;`.
   2. Destructure the body: `let TestLlmConnectionRequest { model, embedding_model, adapter, api_key, base_url, structured_output, extra_headers } = body;` and build `let draft = DraftFields { adapter, api_key, base_url, structured_output, extra_headers };`.
   3. `let mut errors = Vec::new(); draft.check(&mut errors);` — non-empty → `Err(ApiError::BadRequest { detail: None, errors })`. This runs before the limiter (an invalid body spends no test) and before the row load, so an invalid body for an unknown id answers 400, not 404. That is intended: the body is checked first, as `update_llm_connection` does.
   4. The rate-limit check exactly as today.
   5. Load the row (404 as today). `let is_draft = draft.differs_from(&row);`.
   6. `let saved = state.connections.get(&state.secrets, &row).map_err(internal)?;` then `let connection: Arc<cetra_eval::JudgeConnection> = if is_draft { Arc::new(draft.apply((*saved).clone())?) } else { saved };` (the cache is never given a draft).
   7. `let anthropic = draft.adapter.map_or(row.adapter == LlmAdapter::Anthropic.as_str(), |adapter| adapter == LlmAdapter::Anthropic);`
   8. If `trimmed_model(embedding_model.as_deref())` is `Some(embedding_model)`:
      - `anthropic` → `result = anthropic_embedding_refusal(embedding_model)`, nothing stored;
      - else `result = run_embedding_test(&state, access.org.id, &connection, embedding_model).await?`, then `let now = state.clock.now_ns();` (after the call, as today); then, only when `!is_draft`: when `result.ok`, today's `recover_connection(&state, id, row.status != "ok", false, true, now)` with its `after_connection_write(…, "embedding test")` and warn on error (moved from `test_embedding`); then `record_test(&state, id, row.updated_at, result.ok, now).await` (success or failure).
      - Audit: `tracing::info!(target: "audit", connection_id = %id, org_id = %access.org.id, user_id = %user.id, ok = result.ok, latency_ms = result.latency_ms, draft = is_draft, "llm connection tested");` and return `Ok(error::Json(result))`.
   9. Chat: `let explicit = trimmed_model(model.as_deref());` then the model resolution and its 400 exactly as today. `let (result, pause_class) = run_chat_test(&state, access.org.id, &connection, row.id, model).await?;`.
   10. `let now = state.clock.now_ns();` after the call, as today. Only when `!is_draft`: today's `status_write` block unchanged, then S1's `if explicit.is_none() { record_test(…) }`. A draft writes nothing.
   11. Audit as today, plus `draft = is_draft`.

7. Add `test_unsaved_connection` (doc: "Runs one boolean judge or embedding call through a connection that is not saved, and stores nothing (D-I-53)."), same extractor list as `test_connection` without the `Path`, body `error::Json<TestUnsavedLlmConnectionRequest>`. Order:
   1. `refuse_outbound(&state, "connection test")?;`, `session_user`.
   2. Destructure the body. `let embedding_model = trimmed_model(embedding_model.as_deref()); let model = trimmed_model(model.as_deref());`
   3. Errors, in this order: `check_api_key(&api_key, …)`, `check_base_url(base_url.as_deref(), …)`, `check_extra_headers(&extra_headers, …)`, then when both models are `None`: `field_error("model", UNSAVED_MODEL_MESSAGE)`. Non-empty → 400.
   4. Rate-limit check as in `test_connection`.
   5. Build the connection: `let base = cetra_eval::JudgeConnection { adapter: eval_adapter(adapter)?, base_url: None, api_key: cetra_eval::Secret::new(api_key), extra_headers: Vec::new(), structured_output: cetra_eval::StructuredOutput::Native };` then `let connection = DraftFields { adapter: None, api_key: None, base_url, structured_output, extra_headers: Some(extra_headers) }.apply(base)?;`.
   6. Embedding model given: `adapter == LlmAdapter::Anthropic` → refusal; else `run_embedding_test(&state, access.org.id, &connection, embedding_model)`. Else chat: `run_chat_test(&state, access.org.id, &connection, Uuid::nil(), model)` where `model` is the `Some` value from step 2 (it is `Some` here because of step 3; destructure with `let Some(model) = model else { return Err(…same 400…) }` instead of `expect`).
   7. Audit: `tracing::info!(target: "audit", org_id = %access.org.id, user_id = %user.id, ok = result.ok, latency_ms = result.latency_ms, "llm connection draft tested");`. Return the result. No write of any kind.

8. `crates/server/src/api/openapi.rs`:
   1. Import `TestUnsavedLlmConnectionRequest`; `set.insert::<TestUnsavedLlmConnectionRequest>();` right after `set.insert::<TestLlmConnectionRequest>();`.
   2. In `connection_paths`: the `test` operation's description becomes `"Organization admins and owners. Draft fields test values that are not saved; such a test stores nothing."`. Before `let test = …`, add:
      ```rust
      let test_unsaved = operation(
          "post_unsaved_llm_connection_test",
          "llm_connections",
          "Test an unsaved LLM connection",
      )
      .description(Some("Organization admins and owners. Nothing is stored."))
      .parameters(Some(vec![org.clone()]))
      .request_body(Some(json_body("TestUnsavedLlmConnectionRequest")))
      .responses(responses(
          "200",
          json_of::<LlmConnectionTestResult>(
              "The outcome. A provider failure is ok: false, not an error status.",
          ),
          &[400, 401, 403, 404, 429],
      ))
      .build();
      ```
      and add `.path("/api/v1/orgs/{org}/llm-connections/test", PathItem::new(HttpMethod::Post, test_unsaved))` to the returned `paths` chain.

   3. `crates/server/tests/openapi.rs`: in the operation table, right after the `("post", "/api/v1/orgs/{org}/llm-connections/{id}/test", "post_llm_connection_test")` entry (line ~207–211), add `("post", "/api/v1/orgs/{org}/llm-connections/test", "post_unsaved_llm_connection_test"),` in the same multi-line layout; in the expected-errors chain (line ~1440), change `} else if *id == "get_llm_connection" || *id == "post_llm_connection_test" {` to also match `|| *id == "post_unsaved_llm_connection_test"`, so its set is `["400", "401", "403", "404", "429"]`. The route has no connection id, but `404` stays: an unknown or hidden `{org}` answers 404 on every org route, and step 8.2 declares it.

9. `web/src/mocks/writes.ts`: add to `WRITE_OPERATIONS` right after the `post_llm_connection_test` entry
   ```ts
   {
     operationId: "post_unsaved_llm_connection_test",
     method: "post",
     path: "/api/v1/orgs/{org}/llm-connections/test",
   },
   ```
   and to the responses map right after `post_llm_connection_test: …`: `post_unsaved_llm_connection_test: () => HttpResponse.json(llmConnectionTestDocument),`.

10. Regenerate: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just openapi`.

11. `docs/spec/09-api.md` line 72: replace the whole row with:
    ```
    | `GET/POST /api/v1/orgs/{org}/llm-connections` · `GET/PATCH/DELETE …/{id}` · `POST …/{id}/test` · `POST …/llm-connections/test`: org-wide connections; API keys and header values are write-only, and responses carry `api_key_hint` and `extra_header_names`, and `last_tested_at` and `last_test_ok` once the saved values were tested (D-I-54); a PATCH without `api_key` keeps the saved key, also when it changes `base_url` or `adapter` (D-I-53); a PATCH that changes `adapter`, `base_url` or `structured_output`, or sends `api_key` or `extra_headers`, clears the last test. `…/{id}/test` body: `model`, or `embedding_model`, which embeds `"ping"` and is refused with `permanent` for Anthropic (a success resumes the issue sources paused for the connection), plus the draft fields `adapter`, `api_key`, `base_url`, `structured_output` and `extra_headers` with PATCH's semantics and checks (an absent `api_key` is the saved key); a test with a draft field that differs from the saved value, or with `api_key` or `extra_headers`, stores nothing. `…/llm-connections/test` tests an unsaved connection: `adapter`, `api_key`, `base_url?`, `structured_output?`, `extra_headers?` with the create checks, and `model` or `embedding_model`; it stores nothing. Both answer `{ok, model, latency_ms, error?}`, share the per-user limit of connection tests (`429`) and are `503` in restore mode (D-I-4, D-I-5, D-I-53) |
    ```
    (The table has one column, `Method & path`: the row is one cell, as the rows around it.)

12. `docs/spec/06-evaluation.md`: line 148, replace the sentence "A successful connection test or a key change resumes `connection` pauses." (find it by its first words) with "A successful test of the connection's saved values on its default model, or a key change, resumes `connection` pauses; a draft test changes nothing (D-I-53)." Line 207, replace "A successful connection test resumes the sources paused for that connection." with "A successful test of the connection's saved values resumes the sources paused for that connection; a draft test changes nothing (D-I-53)."

**Tests** (`routes/connections.rs` tests module; add a helper `async fn test_unsaved(app: &TestApp, cookie: &str, body: &Value) -> TestResponse` posting to `/api/v1/orgs/acme/llm-connections/test`, like `test`):
- `draft_test_uses_the_saved_key_when_api_key_is_omitted`: `mocked`; `test(…, {"structured_output": "tool"})` → 200; the mock's request has `authorization: Bearer sk-test-key-0123456789`; `get` → `last_tested_at` `None` (a draft).
- `draft_test_uses_a_new_key`: `{"api_key": "sk-draft-key-0123456789"}` → the request's authorization is the draft key; `stored_secret(&app, id)` is the same bytes before and after.
- `a_draft_test_changes_no_status_pause_or_last_test`: `paused_rules(&app, &seeded, id)` as in `a_successful_test_resumes_connection_pauses`; set the status failing through `connections::set_connection_status(conn, id, "failing", Some("bad key"), now)`; draft `{"structured_output": "tool"}` ok → `get`: status `Failing`, `last_error` `Some`, `last_tested_at` `None`; `paused_reason_of(by_connection) == Some(PAUSE_CONNECTION)`.
- `a_failed_draft_test_keeps_the_status`: mock replies 401; draft `{"api_key": "sk-draft-key-0123456789"}` → `ok: false`, class `Pause`; `get`: status `Ok`, `last_error` `None`.
- `a_test_with_the_saved_values_is_not_a_draft`: `{"structured_output": "native"}` → `last_test_ok == Some(true)`.
- `draft_test_with_a_new_base_url_uses_the_saved_key` (Q1): two mocks, `first` and `second`, each `MockLlm::start(vec![completion(r#"{"value": true}"#)])`; `mocked(&app, &cookie, &first)`; `test(…, {"base_url": second.base_url()})` → 200, ok; `second.requests()` has one request with `authorization: Bearer sk-test-key-0123456789`; `first.requests()` is empty; `get` → `base_url` is still `first.base_url()` and `last_tested_at` is `None` (a draft).
- replace the existing `patch_of_the_host_needs_the_key_again` by `patch_of_the_host_keeps_the_saved_key` (Q1): `create(&app, &cookie, &body("Stored"))`; read `stored_secret(&app, id)` and the hint; for each of `{"base_url": "https://proxy.example.com/v1"}`, `{"base_url": ""}`, `{"adapter": "anthropic"}` the PATCH answers 200, `api_key_hint` is unchanged and `stored_secret(&app, id)` is the same bytes; after them `get` has `base_url` `None` and adapter `anthropic`.
- `a_patched_base_url_without_a_key_is_called_with_the_saved_key` (Q1): two mocks as above; `mocked` with `first`; `patch {"base_url": second.base_url()}` → 200; `test(…, {})` → ok; `second` received `Bearer sk-test-key-0123456789`, `first` nothing (the cache was dropped).
- `no_key_material_in_logs` (existing) passes unchanged.
- `draft_test_rejects_an_invalid_base_url_and_headers`: `{"api_key": "sk-draft-key-0123456789", "base_url": "ftp://x", "extra_headers": {"content-type": "x"}}` → 400 with fields `base_url` (`BASE_URL_MESSAGE`) and `extra_headers.content-type`; no request; a following valid `test(…, {})` is still allowed (the 400 spent no rate-limit slot: run it 10 times after the 400, all 200).
- `an_invalid_draft_for_an_unknown_id_is_400`: `test(&app, &cookie, Uuid::now_v7(), &json!({"base_url": "ftp://x"}))` → 400 on `base_url`; the same id with `{}` → 404.
- `a_draft_embedding_test_as_anthropic_is_refused`: saved `openai`; `{"adapter": "anthropic", "api_key": "sk-ant-draft-0123456789", "embedding_model": "e"}` → `ok: false`, `Permanent`, message `NO_EMBEDDINGS_MESSAGE`; no request.
- `unsaved_test_calls_the_provider_with_the_body_values`: `egress_app`; body `{"adapter": "openai", "api_key": "sk-unsaved-0123456789", "base_url": mock.base_url(), "model": "gpt-test", "extra_headers": {"X-Extra": "one"}}` → ok; authorization `Bearer sk-unsaved-0123456789`, header `x-extra: one`, body model `gpt-test`; `GET /api/v1/orgs/acme/llm-connections` has an empty `data`.
- `unsaved_test_validates_like_create`: `{"adapter": "openai", "api_key": "", "base_url": "ftp://x", "extra_headers": {"host": "x"}, "model": "m"}` → 400 with fields `api_key`, `base_url`, `extra_headers.host`; no request.
- `unsaved_test_needs_a_model`: `{"adapter": "openai", "api_key": "sk-unsaved-0123456789"}` → 400, field `model`, message `UNSAVED_MODEL_MESSAGE`.
- `unsaved_embedding_test_on_anthropic_is_refused`: `{"adapter": "anthropic", "api_key": "sk-ant-unsaved-0123456789", "embedding_model": "e"}` → refusal; no request.
- `unsaved_test_to_a_private_address_is_refused_without_the_setting`: `app()` (no egress), mock base URL → `ok: false`, `Permanent`; no request.
- `draft_and_saved_tests_share_the_rate_limit`: `egress_app`, `mocked`; 5 × `test(…, {})` and 5 × `test_unsaved(…)` with the body of `unsaved_test_calls_the_provider_with_the_body_values` → 200; the 11th (`test_unsaved`) → 429 (the manual clock does not move, as in `tests_are_rate_limited`).
- `member_cannot_test_an_unsaved_connection`: as `member_cannot_test` → 403, no request.
- `unsaved_test_is_refused_in_restore_mode`: as `test_connection_is_refused_in_restore_mode`, with `crate::http::restore::tests::assert_refused(&response, "connection test")`.
- `no_key_material_in_logs_of_draft_tests`: `app()`; inside `capture_logs`, a draft `test(…, {"api_key": "sk-logdraft-one-01234567", "extra_headers": {"x-a": "hdr-secret-1"}})` and a `test_unsaved(…)` with `"api_key": "sk-logdraft-two-01234567"`, `"base_url": "https://logcheck.example.com/v1"`, `"extra_headers": {"x-a": "hdr-secret-2"}`, `"model": "m"`; logs contain none of the four secrets nor `logcheck.example.com`, and contain `llm connection tested` and `llm connection draft tested`.
- Every existing test of the file passes unchanged (`test_uses_the_patched_key`, `a_401_marks_the_connection_failing`, `tests_are_rate_limited`, `an_embedding_test_on_anthropic_is_refused_without_a_call`, …).

**Done when:**
```
export CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target
cargo fmt -p cetra-server
cargo clippy -p cetra-server --all-targets --all-features -- -D warnings
cargo test -p cetra-server --all-features --lib -- routes::connections:: http::access_suite:: api_examples::
cargo test -p cetra-server --all-features --test openapi --test api_examples
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
node_modules/.bin/biome check --write web/src/mocks/writes.ts
(cd web && ./node_modules/.bin/vitest run src/mocks/writes.test.ts --maxWorkers=8)
```

**Commit:** `Test draft and unsaved LLM connections, keep the key on a new host`

---

## S3: the assistant's base URL

**Goal:** add `base_url` to `AssistantStatus` so the assistant panel can tell OpenAI from OpenAI-compatible (Q3).

**Edit:**
- `crates/server/src/api/assistant.rs`
- `crates/server/src/routes/assistant.rs`
- `docs/spec/09-api.md`
- generated, by step 4 only: `crates/server/openapi.json`, `web/src/api/schema.gen.ts`

**Steps:**

1. `crates/server/src/api/assistant.rs`, `AssistantStatus`: after `adapter`, add exactly
   ```rust
   /// The connection's base URL, when `usable` and set: it tells OpenAI from an
   /// OpenAI-compatible server (D-I-51).
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub base_url: Option<String>,
   ```
2. `crates/server/src/routes/assistant.rs`: `unusable()` sets `base_url: None`; the `Resolved::Usable` arm of `get_project_assistant` sets `base_url: connection.base_url` (the struct literal moves fields out of `connection`; each field is moved once, so the order does not matter). Run `rtk proxy grep -rn "AssistantStatus {" crates/server/src`; every literal gets the field; any literal other than these two: STOP and ask.
3. `docs/spec/09-api.md`, § Assistant, the `GET /projects/{project}/assistant` row: replace "with the connection's name, adapter and model" with "with the connection's name, adapter, base URL (when set) and model".
4. Regenerate: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just openapi`. Then run `just api-examples`: `fixtures/api/responses/project-assistant.json` must not change (its connection has no base URL); if it does, STOP and ask.

**Tests** (`routes/assistant.rs` tests module, with its helpers `provider`, `mock_app`, `connection`, `put_settings`, `read_status`):
- extend `usable_status_names_the_connection` with `assert_eq!(status["base_url"], base);` (its connection is created with `Some(&base)`).
- `status_reasons`, `restore_mode_is_its_own_reason` and `deleting_the_connection_turns_it_off` pass unchanged: they compare the whole JSON, so they prove an unusable status has no `base_url`. (A usable status without a base URL is not tested at route level: saving the setting calls the provider, which needs the mock's base URL; `skip_serializing_if` covers it.)

**Done when:**
```
export CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target
cargo fmt -p cetra-server
cargo clippy -p cetra-server --all-targets --all-features -- -D warnings
cargo test -p cetra-server --all-features --lib -- routes::assistant:: api_examples::
cargo test -p cetra-server --all-features --test openapi --test api_examples
```
`git status` shows only the **Edit** files and the two generated ones.

**Commit:** `Return the assistant connection's base URL`

---

## W1: provider labels

**Goal:** one `providerLabel` for every place that shows a connection's provider, except the form (W2).

**Edit:**
- `web/src/lib/llm-adapters.ts`
- `web/src/lib/llm-adapters.test.ts` (new)
- `web/src/routes/$org/settings/llm-connections.tsx`
- `web/src/routes/$org/settings/llm-connections.test.tsx`
- `web/src/components/assistant/assistant-panel.tsx`
- `web/src/components/assistant/assistant-panel.test.tsx`

**Steps:**

1. `web/src/lib/llm-adapters.ts`: keep `LLM_ADAPTER_LABELS` (the form still uses it until W2) and add exactly:

```ts
/** The connection form's four provider choices over the three adapters (D-I-51). */
export type ProviderChoice = "openai" | "openai_compatible" | "anthropic" | "gemini";

export const PROVIDER_CHOICES: readonly { value: ProviderChoice; label: string }[] = [
  { value: "openai", label: "OpenAI" },
  { value: "openai_compatible", label: "OpenAI-compatible" },
  { value: "anthropic", label: "Anthropic" },
  { value: "gemini", label: "Gemini" },
];

/** The choice a connection shows as: `openai` with a base URL is OpenAI-compatible. */
export function providerChoice(connection: {
  adapter: Schema<"LlmAdapter">;
  base_url?: string | null;
}): ProviderChoice {
  if (connection.adapter === "openai") {
    return connection.base_url ? "openai_compatible" : "openai";
  }
  return connection.adapter;
}

/** The provider's display name: OpenAI, OpenAI-compatible, Anthropic or Gemini. */
export function providerLabel(connection: {
  adapter: Schema<"LlmAdapter">;
  base_url?: string | null;
}): string {
  const choice = providerChoice(connection);
  return PROVIDER_CHOICES.find((option) => option.value === choice)?.label ?? choice;
}
```

2. `web/src/routes/$org/settings/llm-connections.tsx`: replace the six uses of `LLM_ADAPTER_LABELS[connection.adapter]` / `LLM_ADAPTER_LABELS[row.original.adapter]` (the Provider column's `accessorFn` and `cell`, `SORT_KEYS.provider`, the `filterRows` fields, the `ConnectionDetail` subtitle, `ReadOnlyDetails`) with `providerLabel(connection)` / `providerLabel(row.original)`; import `providerLabel` instead of `LLM_ADAPTER_LABELS`.

3. `web/src/components/assistant/assistant-panel.tsx`: replace `adapterLabel` with

```ts
/** The provider's display name; the status route returns the adapter as a plain string. */
function adapterLabel(adapter: string, baseUrl: string | undefined): string {
  return adapter === "openai" || adapter === "anthropic" || adapter === "gemini"
    ? providerLabel({ adapter, base_url: baseUrl })
    : adapter;
}
```

   its call becomes `adapterLabel(status.data.adapter, status.data.base_url)`; import `providerLabel` instead of `LLM_ADAPTER_LABELS`.

**Tests:**
- `web/src/lib/llm-adapters.test.ts`: `providerChoice` of `{adapter: "openai"}` → `openai`; `{adapter: "openai", base_url: ""}` → `openai`; `{adapter: "openai", base_url: "https://gw.example.com/v1"}` → `openai_compatible`; `anthropic` → `anthropic`; `gemini` → `gemini`; `providerLabel` of each gives "OpenAI", "OpenAI", "OpenAI-compatible", "Anthropic", "Gemini".
- `llm-connections.test.tsx`:
  - update `lists connections with provider, model and status`: the fixture has no base URL, so the grid shows "OpenAI" twice (name and provider): `expect(within(grid).getAllByText("OpenAI")).toHaveLength(2)`; drop the "OpenAI-compatible" assertion.
  - new `labels an OpenAI connection with a base URL as OpenAI-compatible`: override `GET /api/v1/orgs/:org/llm-connections` with `{ ...CONNECTION, base_url: "https://gw.example.com/v1" }`; the grid shows "OpenAI-compatible"; after opening the row (`openConnection`), the panel subtitle shows it: `within(screen.getByRole("heading", { level: 2, name: CONNECTION_NAME }).parentElement as HTMLElement).getByText("OpenAI-compatible")` (`SettingsPanel` puts the title `h2` and the subtitle in one wrapper; the form card's own `h2` is named "Connection").
  - `filters the list by name, provider or model` and `a member sees the list and the details, but cannot manage` pass unchanged.
- `assistant-panel.test.tsx`: new `names an OpenAI-compatible server in the connection notice`: `server.use(http.get(STATUS, () => HttpResponse.json({ usable: true, connection_name: "Gateway", adapter: "openai", base_url: "https://gw.example.com/v1", model: "gpt-4o" })))`, open the panel as in the first test; `findByText(/Answers use/)` has text content "OpenAI-compatible".

**Done when:**
```
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
node_modules/.bin/biome check --write web/src/lib/llm-adapters.ts web/src/lib/llm-adapters.test.ts 'web/src/routes/$org/settings/llm-connections.tsx' 'web/src/routes/$org/settings/llm-connections.test.tsx' web/src/components/assistant/assistant-panel.tsx web/src/components/assistant/assistant-panel.test.tsx
(cd web && ./node_modules/.bin/vitest run src/lib/llm-adapters.test.ts 'src/routes/$org/settings/llm-connections.test.tsx' src/components/assistant/assistant-panel.test.tsx --maxWorkers=8)
pnpm --dir web run typecheck
```
`git status` shows only this phase's **Edit** files. `pnpm --dir web run typecheck` runs `tsr generate` first, which rewrites `web/src/routeTree.gen.ts`; that file is git-ignored (`.gitignore` line 13), so it never shows. `pnpm` is not in the nvm bin dir: it is `$HOME/.npm-global/bin/pnpm`; if `pnpm` is not found, `export PATH=$HOME/.npm-global/bin:$PATH`.

**Commit:** `Label LLM connection providers as OpenAI or OpenAI-compatible`

---

## W2: the provider-aware connection form

**Goal:** the form's layout, provider choices, placeholders, hints and model suggestions of D-I-51 and D-I-52.

**Edit:**
- `web/src/components/settings/connection-form.tsx`
- `web/src/components/form/text-field.tsx`
- `web/src/lib/llm-adapters.ts`
- `web/src/lib/llm-adapters.test.ts`
- `web/src/lib/llm-models.ts` (new)
- `web/src/lib/llm-models.test.ts` (new)
- `web/src/routes/$org/settings/llm-connections.test.tsx`

**Steps:**

1. `web/src/lib/llm-adapters.ts`: delete `LLM_ADAPTER_LABELS` (W1 left the form as its only user) and add exactly:

```ts
/** The adapter a choice saves: OpenAI-compatible is `openai` with a base URL. */
export function adapterOf(choice: ProviderChoice): Schema<"LlmAdapter"> {
  return choice === "openai_compatible" ? "openai" : choice;
}

/** Each adapter's default base URL (`DEFAULT_BASE` in `crates/eval/src/chat/*.rs`). */
export const DEFAULT_BASE_URLS: Record<Schema<"LlmAdapter">, string> = {
  openai: "https://api.openai.com/v1",
  anthropic: "https://api.anthropic.com",
  gemini: "https://generativelanguage.googleapis.com",
};
```

   Create `web/src/lib/llm-models.ts` exactly (every id is a row of `crates/otel/prices/builtin.json` with the provider `openai`, `anthropic` or `gcp.gemini`; the order is the order shown):

```ts
import type { ProviderChoice } from "@/lib/llm-adapters";

/**
 * Model ids the Default model field suggests for each provider choice (D-I-52). A short static
 * list: the field takes any other id too. OpenAI-compatible servers name their models
 * themselves, so they get none.
 */
export const MODEL_SUGGESTIONS: Record<ProviderChoice, readonly string[]> = {
  openai: [
    "gpt-5",
    "gpt-5-mini",
    "gpt-5-nano",
    "gpt-4.1",
    "gpt-4.1-mini",
    "gpt-4o",
    "gpt-4o-mini",
    "o3",
    "o4-mini",
  ],
  openai_compatible: [],
  anthropic: [
    "claude-opus-4-7",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
  ],
  gemini: [
    "gemini-3.1-pro-preview",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
  ],
};
```

2. `web/src/components/form/text-field.tsx`: add an optional prop `list?: string` (doc: "The id of a `<datalist>` of suggestions.") passed to `<Input list={list} …>`.

3. `connection-form.tsx`, state: replace `adapter` state with `const [provider, setProvider] = useState<ProviderChoice>(connection ? providerChoice(connection) : "openai");` and `const adapter = adapterOf(provider);`. Delete `PROVIDERS`, `adapterValue` and the `LLM_ADAPTER_LABELS` import; import `PROVIDER_CHOICES`, `providerChoice`, `adapterOf`, `DEFAULT_BASE_URLS`, `type ProviderChoice` from `@/lib/llm-adapters`, and `MODEL_SUGGESTIONS` from `@/lib/llm-models`. A `providerValue(value: string): ProviderChoice` helper returns `value` when it is one of the four choices, else `"openai"`.

4. Provider change handler (exact):

```ts
  function chooseProvider(next: ProviderChoice) {
    setProvider(next);
    // Another provider's URL is wrong for this one; the saved choice gets its saved URL back.
    setBaseUrl(
      connection !== undefined && next === providerChoice(connection)
        ? (connection.base_url ?? "")
        : "",
    );
    touch("provider");
  }
```

5. Client errors: add, after the API key rule, `if (provider === "openai_compatible" && baseUrl.trim().length === 0) { clientErrors.base_url = "Enter the server's base URL."; }`.

6. No key rule on an edit (Q1): a change of provider or base URL without a new key is valid; the PATCH keeps the saved key. Add no client error for it.

7. Model suggestions: `const suggestions = MODEL_SUGGESTIONS[provider];` (no request).

8. Render, inside `SettingsFormCard`, one wrapper `<div className="flex max-w-xl flex-col gap-4">` holding, in this order:
   1. **Provider**: the existing `Label` + `Select` markup with `value={provider}`, `onValueChange={(value) => chooseProvider(providerValue(value))}`, options from `PROVIDER_CHOICES`; remove `aria-describedby` and the description paragraph (no provider hint).
   2. **Name**: the existing `TextField`.
   3. **API key**: the existing `TextField` with `placeholder={connection === undefined ? undefined : connection.api_key_hint ? \`••••${connection.api_key_hint}\` : "••••••••"}` and `description={connection === undefined ? "Stored encrypted, never shown again." : "Stored encrypted. Type a new key to replace it."}`.
   4. **Base URL**, only when `provider === "openai_compatible"`: `TextField` `id="connection-base-url"`, label "Base URL", `placeholder="https://my-gateway.example.com/v1"`, `description="Any server with an OpenAI-style API: Azure OpenAI, vLLM, Ollama, OpenRouter, LiteLLM."`, value/error/onChange as today.
   5. **Default model**: the existing `TextField` with `list={suggestions.length > 0 ? "connection-model-suggestions" : undefined}`, followed, when `suggestions.length > 0`, by `<datalist id="connection-model-suggestions">{suggestions.map((name) => <option key={name} value={name} />)}</datalist>`.
   6. **Advanced** `<details>`, its body in this order: Base URL when `provider !== "openai_compatible"` (`TextField` `id="connection-base-url"`, label "Base URL", `placeholder={DEFAULT_BASE_URLS[adapter]}`, `description="Only for a proxy. Leave empty to use the provider's API."`); then the existing Structured output block (label, select, its hint "Use Tool call or Prompt only if the provider rejects native structured output."); then the existing concurrency row and Extra headers fieldset, unchanged.
9. `advancedError` gains `|| (provider !== "openai_compatible" && fieldError("base_url") !== undefined)`.
10. `editBody` and `createBody` keep their logic, with `adapter` now `adapterOf(provider)`; `dirty` for a create form also counts `provider !== "openai"`.

**Tests** (`llm-connections.test.tsx`; pick an option as the existing structured-output test does: click the combobox, then `screen.findByRole("option", { name })`):
- update `sends one extra header and the picked structured output mode`: click "Advanced" before the Structured output combobox.
- update `shows a server field error under its field`: also expect `within(form).getByText("Advanced").closest("details")` to have attribute `open` (the default OpenAI choice puts Base URL in Advanced).
- `shows a saved connection without a base URL as OpenAI`: open the fixture; `within(form).getByRole("combobox", { name: "Provider" })` has text content matching `/^OpenAI$/`; `within(form).getByLabelText("Base URL").closest("details")` is not null; its placeholder is `https://api.openai.com/v1`.
- `shows a saved connection with a base URL as OpenAI-compatible`: list override with `base_url: "https://gw.example.com/v1"`; Provider shows "OpenAI-compatible"; Base URL has that value, `closest("details")` is null, and its description is the compatible hint.
- `requires a base URL for OpenAI-compatible`: create form; choose "OpenAI-compatible"; type Name and API key; "Add connection" is disabled; type then clear Base URL → "Enter the server's base URL." shows; type `https://gw.example.com/v1` and save → body `{ name, adapter: "openai", api_key, base_url: "https://gw.example.com/v1" }`.
- `switching to OpenAI clears the base URL in the PATCH body`: list override with a base URL; choose "OpenAI"; Save is enabled without a key; Save → PATCH body `{ base_url: "" }` (Q1).
- `restores the saved base URL when the saved provider is picked again`: list override with a base URL; choose "Anthropic", then "OpenAI-compatible"; Base URL has the saved value and the form is not dirty (no "Unsaved changes").
- `shows the saved key's last four characters as the placeholder`: fixture → API key placeholder `••••WXYZ` and the description "Stored encrypted. Type a new key to replace it."; list override with `api_key_hint: ""` → placeholder `••••••••`.
- `puts structured output and the Base URL of other providers in Advanced`: the Structured output combobox's `closest("details")` is not null; choose "Anthropic" → Base URL placeholder `https://api.anthropic.com`; "Gemini" → `https://generativelanguage.googleapis.com`.
- `orders the fields Provider, Name, API key, Default model`: for each consecutive pair of the controls (Provider combobox, Name, API key, Default model), `a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING` is non-zero.
- `caps the form's fields at a readable width`: `within(form).getByLabelText("Name").closest(".max-w-xl")` is not null.
- `suggests model ids of the chosen provider`: create form: Default model has attribute `list="connection-model-suggestions"` and the datalist options' values (read with `document.querySelectorAll("#connection-model-suggestions option")`) equal `MODEL_SUGGESTIONS.openai`; choose "Anthropic" → `MODEL_SUGGESTIONS.anthropic`; "Gemini" → `MODEL_SUGGESTIONS.gemini`; choose "OpenAI-compatible" → no `list` attribute and no datalist.
- `web/src/lib/llm-adapters.test.ts`: `adapterOf` of each choice; `DEFAULT_BASE_URLS` holds the three URLs above.
- `web/src/lib/llm-models.test.ts`: `MODEL_SUGGESTIONS.openai_compatible` is empty; every other list is non-empty, has no duplicates, and every id has no whitespace and none of `/ \ % ? #` (the connection's model rule).
- Every other existing test of the file passes (update a selector only where a field moved into Advanced; report each such change).

**Done when:**
```
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
node_modules/.bin/biome check --write web/src/components/settings/connection-form.tsx web/src/components/form/text-field.tsx web/src/lib/llm-adapters.ts web/src/lib/llm-adapters.test.ts web/src/lib/llm-models.ts web/src/lib/llm-models.test.ts 'web/src/routes/$org/settings/llm-connections.test.tsx'
(cd web && ./node_modules/.bin/vitest run src/lib/llm-adapters.test.ts src/lib/llm-models.test.ts 'src/routes/$org/settings/llm-connections.test.tsx' src/components/form --maxWorkers=8)
pnpm --dir web run typecheck
```
`git status` shows only this phase's **Edit** files. `pnpm --dir web run typecheck` runs `tsr generate` first, which rewrites `web/src/routeTree.gen.ts`; that file is git-ignored (`.gitignore` line 13), so it never shows. `pnpm` is not in the nvm bin dir: it is `$HOME/.npm-global/bin/pnpm`; if `pnpm` is not found, `export PATH=$HOME/.npm-global/bin:$PATH`.
By hand (`just web-dev`, `/acme/settings/llm-connections`, open the row): the order, the placeholders, the datalist, the width cap at a wide window.

**Commit:** `Make the LLM connection form provider-aware`

---

## W3: test from the form and the last-test header

**Goal:** the Test button tests the form's current values with the result under the form; the embedding test moves into Advanced; the Test dialog goes; the header shows the last test.

**Edit:**
- `web/src/components/settings/connection-form.tsx`
- `web/src/components/settings/connection-test.tsx`
- `web/src/routes/$org/settings/llm-connections.tsx`
- `web/src/routes/$org/settings/llm-connections.test.tsx`

**Steps:**

1. `connection-test.tsx`: delete `useTestConnection`, the `TestConnection` type, `ConnectionTest` and `ConnectionTestForm`. Keep `HINTS` and `ConnectionTestResult` (the assistant settings page uses it). The file's imports become exactly one line, `import type { Schema } from "@/api/types";` (`HINTS` and `ConnectionTestResult` use only `Schema`; JSX needs no React import in this project). Every other import goes: `useQueryClient`, `FormEvent`, `useState`, `$api`, `problemMessage`, `problemOf`, `connectionsKey`, `FormError`, `SubmitButton`, `TextField`, `Button` and the `Dialog*` components.

2. `connection-form.tsx`: remove the `onTest` prop and its footer button. Add the mutations (the requests hold the key, so `gcTime: 0` as the create and patch hooks):

```ts
  const queryClient = useQueryClient();
  const testSaved = $api.useMutation("post", "/api/v1/orgs/{org}/llm-connections/{id}/test", {
    gcTime: 0,
  });
  const testUnsaved = $api.useMutation("post", "/api/v1/orgs/{org}/llm-connections/test", {
    gcTime: 0,
  });
  const testing = testSaved.isPending || testUnsaved.isPending;
  const [testResult, setTestResult] = useState<Schema<"LlmConnectionTestResult"> | undefined>();
  const [testError, setTestError] = useState<string | undefined>();
  const [embeddingModel, setEmbeddingModel] = useState("");
```

   `touch(field)` also runs `setTestResult(undefined); setTestError(undefined);` (a result describes the values it tested). The PATCH `onSuccess` in `submit()` also runs `setTestResult(undefined); setTestError(undefined);` (the saved values changed).

   **No stale results:** while a test runs, the form cannot change. Add `const busy = pending || testing;` and use `disabled={busy}` instead of `disabled={pending}` on every field and control of the form: the Provider select, Name, API key, both Base URL fields, Default model, Structured output, Max concurrency, Requests per minute, Add header and each header row's name, value and remove button. Save: pass `disabledReason={testing ? "Wait for the test to finish." : undefined}` to `SettingsFormCard` (keep `pending={pending}`).

3. Test bodies (exact):

   Declare `type TestKind = "chat" | "embedding";` at module level (above `ConnectionForm`), and inside the component:

```ts
  /** A saved connection's test: the fields that differ from it, and the model only when it changed. */
  function savedTestBody(current: Connection, kind: TestKind): Schema<"TestLlmConnectionRequest"> {
    const edits = editBody(current);
    const body: Schema<"TestLlmConnectionRequest"> = {};
    if (edits.adapter !== undefined) body.adapter = edits.adapter;
    if (edits.api_key !== undefined) body.api_key = edits.api_key;
    if (edits.base_url !== undefined) body.base_url = edits.base_url;
    if (edits.structured_output !== undefined) body.structured_output = edits.structured_output;
    if (edits.extra_headers !== undefined) body.extra_headers = edits.extra_headers;
    if (kind === "embedding") {
      body.embedding_model = embeddingModel.trim();
    } else if (defaultModel.trim() !== (current.default_model ?? "").trim()) {
      // An unchanged default model is left out, so the server counts the test as one of the
      // default model (D-I-53).
      body.model = defaultModel.trim();
    }
    return body;
  }

  /** An unsaved connection's test: every value set in the form. */
  function unsavedTestBody(kind: TestKind): Schema<"TestUnsavedLlmConnectionRequest"> {
    const body: Schema<"TestUnsavedLlmConnectionRequest"> = { adapter, api_key: apiKey };
    if (baseUrl.trim()) body.base_url = baseUrl.trim();
    if (structured !== "") body.structured_output = structured;
    if (headersDirty) body.extra_headers = headerValues();
    if (kind === "embedding") {
      body.embedding_model = embeddingModel.trim();
    } else {
      body.model = defaultModel.trim();
    }
    return body;
  }
```

   (Biome may ask for braces on the one-line `if`s; let `biome check --write` format them.)

4. Disabled reasons (exact order):

```ts
  // Only what a test sends can stop it: the name and the limits are not sent.
  const testProblem = clientErrors.api_key ?? clientErrors.base_url ?? clientErrors.headers;
  const chatTestReason =
    testProblem ?? (defaultModel.trim() === "" ? "Enter a default model to test." : undefined);
  const embeddingTestReason =
    adapter === "anthropic"
      ? "Anthropic has no embeddings API"
      : (testProblem ??
        (embeddingModel.trim() === "" ? "Enter an embedding model to test." : undefined));
```

5. `runTest(kind: TestKind)`:
   1. `setTestResult(undefined); setTestError(undefined); setServerErrors({}); setServerError(undefined);`
   2. `const options = { onSuccess: (result) => setTestResult(result), onError: handleTestError };`
   3. Saved: `testSaved.mutate({ params: { path: { org, id: connection.id } }, body: savedTestBody(connection, kind) }, { ...options, onSuccess: (result) => { setTestResult(result); void queryClient.invalidateQueries({ queryKey: connectionsKey(org) }); } })` — the status and the last test may have changed. New: `testUnsaved.mutate({ params: { path: { org } }, body: unsavedTestBody(kind) }, options)`.
   4. `handleTestError` never calls `handleError` (whose fallback says "Could not save the connection."). Exact:

```ts
  function handleTestError(caught: unknown) {
    if (problemOf(caught)?.status === 429) {
      setTestError("Too many tests. Wait a minute.");
      return;
    }
    const placed: Partial<Record<FieldKey, string>> = {};
    const unplaced: string[] = [];
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      if (field.startsWith("extra_headers")) {
        placed.headers ??= message;
      } else if (isPlacedField(field)) {
        placed[field] = message;
      } else {
        // `model`, `embedding_model`: the test's own fields, shown under the form.
        unplaced.push(message);
      }
    }
    setServerErrors(placed);
    if (unplaced.length > 0) {
      setTestError(unplaced.join(" "));
    } else if (Object.keys(placed).length === 0) {
      setTestError(problemMessage(caught, "Could not run the test."));
    }
  }
```

   Import `problemOf` from `@/api/problem`.

6. Render:
   1. Footer (`footerExtra`): `<DisabledReason reason={chatTestReason}><Button type="button" variant="outline" disabled={chatTestReason !== undefined || testing || pending} onClick={() => runTest("chat")}>{testing ? "Testing…" : "Test connection"}</Button></DisabledReason>` — for a new and a saved connection alike.
   2. At the end of Advanced's body: `TextField` `id="connection-embedding-model"`, label "Embedding model", description "Optional. Checks a model that issue sources embed with.", `disabled={busy || adapter === "anthropic"}`, `onChange` sets it and calls `touch("embedding_model")`; then `<DisabledReason reason={embeddingTestReason}><Button type="button" variant="outline" size="sm" disabled={embeddingTestReason !== undefined || testing || pending} onClick={() => runTest("embedding")}>Test embeddings</Button></DisabledReason>`.
   3. After the `max-w-xl` wrapper, still inside the card: `<div aria-live="polite" className="max-w-xl">{testResult ? <ConnectionTestResult result={testResult} /> : null}<FormError message={testError} /></div>` (import `ConnectionTestResult` from `@/components/settings/connection-test` and `FormError` from `@/components/form/form-error`).

7. `llm-connections.tsx`:
   1. Remove the `ConnectionTest` import, `testTarget`/`testOpen` state, `openTest`, the `<ConnectionTest …/>` element, the fragment it needed, and the `onTest` prop of `ConnectionDetail` and of `<ConnectionForm>`. Every other import of the file stays: `useState` is still used by `ConnectionsList` and `ConnectionDetail`, `type Connection` by the columns, sort keys and components, and `RelativeTime` by the list and the new header line.
   2. In `ConnectionDetail`'s subtitle, after the status pill, add exact:

```tsx
          {connection.last_tested_at === undefined ? null : connection.last_test_ok === false ? (
            <span>
              Last test failed <RelativeTime value={connection.last_tested_at} />
            </span>
          ) : (
            <span>
              Tested <RelativeTime value={connection.last_tested_at} />
            </span>
          )}
```

**Tests** (`llm-connections.test.tsx`; delete the five dialog tests `tests a connection from its panel`, `tests with an empty model sends an empty body`, `blocks Test while the connection has unsaved edits`, `shows the rate limit message in the test dialog`, `shows a failed test with its hint`, and add):
- `tests the saved values from the panel`: record the body of `POST /api/v1/orgs/:org/llm-connections/:id/test` (answer `{ ok: true, model: "gpt-5-mini", latency_ms: 420 }`); open; click "Test connection" → body `{}`; `within(form).findByText("Connection works (420 ms, model gpt-5-mini)")`.
- `tests unsaved edits with draft fields and shows the result inline`: type `sk-new-key-0123456789` in API key and change Default model to `gpt-5`; click "Test connection" → body `{ api_key: "sk-new-key-0123456789", model: "gpt-5" }`; the result line is inside the form; no `dialog` role exists.
- `tests a new connection before it is saved`: record `POST /api/v1/orgs/:org/llm-connections/test`; create form; type API key `sk-secret` and Default model `gpt-5-mini` (no name); click "Test connection" → body `{ adapter: "openai", api_key: "sk-secret", model: "gpt-5-mini" }`; result shown; no create request was sent.
- `disables Test without a default model, with its reason`: create form; type API key only; "Test connection" is disabled and its `parentElement` has accessible description "Enter a default model to test."; with no API key the description is "Enter the API key.".
- `tests embeddings from Advanced`: open; click "Advanced"; type `text-embedding-3-small` in "Embedding model"; click "Test embeddings" → body `{ embedding_model: "text-embedding-3-small" }`.
- `refuses the embedding test for Anthropic`: create form; choose "Anthropic"; "Test embeddings" is disabled with accessible description "Anthropic has no embeddings API".
- `shows a failed test with its hint under the form`: answer `{ ok: false, model: "m", latency_ms: 5, error: { class: "pause", message: "provider status 401" } }`; "Test failed", the message and the pause hint are inside the form.
- `shows the rate limit message under the form`: `problemResponse(429, "too_many_requests", { detail: "slow down" })` → "Too many tests. Wait a minute." inside the form.
- `clears the result when a field changes`: after a result, type in Name → the result line is gone.
- `clears the result after a save`: open; type " 2" at the end of Name; click "Test connection" (body `{}`: the name is not a draft field) → the result line shows; click "Save" (PATCH answered with `{ ...CONNECTION, name: "OpenAI 2" }`) → the result line is gone.
- `locks the form while a test runs`: answer the test route with a promise the test resolves itself (`let release!: () => void; const gate = new Promise<void>((resolve) => { release = resolve; });` then `await gate` in the handler before `HttpResponse.json(...)`); click "Test connection"; while pending, Name, API key and Default model are disabled and the button reads "Testing…"; call `release()`; after the result shows, Name is enabled again.
- `shows a test's field error under the form, not as a failed save`: create form; type API key `sk-secret` and Default model `m`; answer `POST /api/v1/orgs/:org/llm-connections/test` with `problemResponse(400, "bad_request", { errors: [{ field: "model", message: "model is required to test an unsaved connection" }] })`; click "Test connection" → `within(form).findByText("model is required to test an unsaved connection")`; `within(form).queryByText("Could not save the connection.")` is null.
- `shows when the saved values were last tested`: list override with `last_tested_at: new Date(Date.now() - 5 * 60_000).toISOString(), last_test_ok: true`; open; `screen.getByText(/^Tested/)` has text content "Tested 5m ago" (`getByText` matches the span by its own text node "Tested "; `toHaveTextContent` reads the whole span, including the `<time>` child "5m ago").
- `shows a failed last test`: same with `last_test_ok: false` → `getByText(/^Last test failed/)` has text content "Last test failed 5m ago".
- `shows nothing when never tested`: list override with the fixture without `last_tested_at` and `last_test_ok` (spread and delete both keys); no text matching `/^Tested/` or `/^Last test failed/`.
- `opens the row's panel and writes its id to the URL` keeps asserting "Test connection" is enabled.
- `web/src/routes/$org/settings/assistant.test.tsx` must still pass (it renders `ConnectionTestResult`).

**Done when:**
```
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
node_modules/.bin/biome check --write web/src/components/settings/connection-form.tsx web/src/components/settings/connection-test.tsx 'web/src/routes/$org/settings/llm-connections.tsx' 'web/src/routes/$org/settings/llm-connections.test.tsx'
(cd web && ./node_modules/.bin/vitest run 'src/routes/$org/settings/llm-connections.test.tsx' 'src/routes/$org/settings/assistant.test.tsx' --maxWorkers=8)
pnpm --dir web run typecheck
```
`git status` shows only this phase's **Edit** files. `pnpm --dir web run typecheck` runs `tsr generate` first, which rewrites `web/src/routeTree.gen.ts`; that file is git-ignored (`.gitignore` line 13), so it never shows. `pnpm` is not in the nvm bin dir: it is `$HOME/.npm-global/bin/pnpm`; if `pnpm` is not found, `export PATH=$HOME/.npm-global/bin:$PATH`.
By hand (`just web-dev`): Test on the edit and the create panels, the result under the form, the header line.

**Commit:** `Test LLM connections from the form and show the last test`

---

## D1: specs 11 and 12, the docs page, changelog, roadmap

**Goal:** the user-facing descriptions match D-I-51 to D-I-54.

**Edit:**
- `docs/spec/11-ui.md`
- `docs/spec/12-operations.md`
- `website/src/content/docs/docs/evaluation/llm-connections.mdx`
- `CHANGELOG.md`
- `docs/roadmap.md`

**Steps:**

1. `docs/spec/11-ui.md` line 246: replace "LLM connections (track I): list for every member; add, edit, delete and test for admins (D-I-4);" with:
   "LLM connections (track I): list for every member; add, edit, delete and test for admins (D-I-4). The panel's form, its fields capped at a readable width, holds Provider (OpenAI, OpenAI-compatible, Anthropic, Gemini), Name, API key (on an edit, the placeholder shows the saved key's last 4 characters), Base URL (required for OpenAI-compatible; for the others under Advanced, with the provider's default URL as placeholder), Default model (free text with suggested model ids for the provider) and Advanced (structured output, limits, extra headers, an embedding model and Test embeddings). Test connection tests the form's current values, saved or not, and shows the result under the form; the panel header shows when the saved values were last tested and whether that test failed. The list's Provider column says OpenAI or OpenAI-compatible from the base URL (D-I-51 to D-I-54);"
2. `docs/spec/12-operations.md` line 163: replace "the LLM connection form explains the setting; the UI cannot read its value (D-I-4)" with "the LLM connections docs page explains the setting; the UI cannot read its value (D-I-4, D-I-51)".
3. `website/src/content/docs/docs/evaluation/llm-connections.mdx` (follow `website/STYLE.md`; add no code block):
   1. § Supported providers: intro "The form offers four choices over three provider APIs:" and the table rows: OpenAI — "OpenAI's API. The base URL is optional, under **Advanced**, for a proxy."; OpenAI-compatible — "Any server with an OpenAI-style chat API: Azure OpenAI, vLLM, Ollama, OpenRouter, LiteLLM. Enter its base URL."; Anthropic — "Messages API."; Gemini — "Google's `generateContent` API.". Then: "Over the API, OpenAI and OpenAI-compatible are both `"adapter": "openai"`; a compatible server sets `base_url`."
   2. § Add a connection, steps: "1. Open the organization's **LLM connections** page and select **Add connection**." "2. Choose the provider, give the connection a name and paste the API key. For OpenAI-compatible, enter the server's base URL." "3. Optional: pick a default model (the field suggests common model ids for the provider) and open **Advanced** for the structured output mode, the limits and extra headers." "4. Select **Test connection** to check the values before you save them. The test makes one small judge call with the default model and shows whether it worked and how long it took. Then save." Keep the curl block as it is.
   3. New `## Test a connection` section, placed after the `### Structured output` section and before `## The assistant`: "**Test connection** checks the values in the form, saved or not. A test of changed values does not touch the saved connection. When you test the saved values with the default model, the panel shows when, as "Tested 5m ago", or "Last test failed" when it did not work. Changing the provider, key, base URL, structured output mode or headers clears that line. Under **Advanced**, **Test embeddings** checks an embedding model for issue sources. Anthropic has no embeddings API."
   4. § How keys are stored: after "To replace a key, edit the connection and enter a new one." add "The key field shows its last 4 characters as a placeholder. If you change the provider or the base URL without entering a new key, cetra keeps the saved key: saving sends it to the new base URL from then on, and **Test connection** sends it there right away."
   5. § Private networks: replace "The UI cannot read that setting, so the connection form only explains it." with "The UI cannot read that setting."
   6. § When a connection fails: replace "A successful test or a new key resumes them." with "A successful test of the saved values with the default model, or a new key, resumes them."
4. `CHANGELOG.md` § `[Unreleased]`:
   - `### Added`: "- LLM connections: **Test connection** checks the form's values before you save them, for a new connection too, and **Test embeddings** under Advanced checks an embedding model. The panel header shows when the saved values were last tested. `POST /api/v1/orgs/{org}/llm-connections/test` tests an unsaved connection, and `POST …/llm-connections/{id}/test` takes draft fields; a connection's `last_tested_at` and `last_test_ok` hold the last test of its saved values."
   - `### Added`: "- The connection form suggests common model ids for OpenAI, Anthropic and Gemini in the Default model field."
   - `### Changed`: "- `PATCH /api/v1/orgs/{org}/llm-connections/{id}` no longer needs `api_key` with a new `base_url` or `adapter`: the saved key is kept."
   - `### Changed`: "- The LLM connection form offers OpenAI, OpenAI-compatible, Anthropic and Gemini; a saved OpenAI connection with a base URL shows as OpenAI-compatible. The form puts the base URL and structured output under Advanced unless the provider needs them, shows the saved key's last 4 characters, and its fields keep a readable width. The Test dialog is gone."
5. `docs/roadmap.md`, track I row (line 50): at the end of the Status cell, before ` | \`D-I-n\` |`, append " The connection form follow-up ([`plans/i-online-eval/connection-form.md`](plans/i-online-eval/connection-form.md): provider choices, draft tests, the stored last test) adds D-I-51–D-I-54."

**Tests:** none (docs). The site's content checks below must pass.

**Done when:**
```
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
(cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-env-vars.mjs)
(cd website && ../node_modules/.bin/biome check src/content/docs/docs/evaluation/llm-connections.mdx || true)
```
(The coordinator's final `just check` runs `just check-docs` with the build and the link validation.)

**Commit:** `Document the LLM connection form, draft tests and the last test`

---

## After the last phase (coordinator)

1. Merge D1 into `claude/connection-form`.
2. Sonnet code review of the whole branch against `origin/main` (fetch first).
3. `just check` once; `just e2e` is not needed (no SDK change).
4. Look at the panel on the mocks (`just web-dev`) at a wide window: the width cap, the order, the Advanced contents for each provider, Test on create and edit, the header line.
