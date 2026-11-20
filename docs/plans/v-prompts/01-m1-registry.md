# V · M1 — Prompt registry: template syntax, tables, store, API

Implementation plan for milestone M1 of track V. It has 5 phases, **V1.1 to V1.5**, one commit each, done in order by one agent at a time. **Haiku** implements each phase with no context beyond this plan, the design doc and the files it names; **Sonnet** reviews the milestone. Nothing here may need a guess: if this plan and the code disagree (a name moved, a function changed, a helper is missing), **STOP and report** instead of guessing. Each phase ends with `just check` green.

**Prerequisites** (check before V1.1). Each command must print at least one line, else STOP:

```bash
grep -n "pub(crate) fn created_by" crates/server/src/routes/datasets.rs
grep -n "pub(crate) fn page_named" crates/server/src/routes/orgs.rs
grep -n "fn dataset_paths" crates/server/src/api/openapi.rs
grep -n "pub(crate) async fn seed_example_faq_dataset" crates/server/src/test_support/mod.rs
ls crates/server/migrations/meta/0022_dashboard_kind.sql
```

`docs/plans/v-prompts/00-design.md` must exist and list D-V-1 to D-V-28.

**Read first** (every phase):
- `AGENTS.md`, all of it.
- `docs/plans/v-prompts/00-design.md`: § Tables, § API, § Decisions D-V-2 to D-V-13.
- This file up to § V1.1, then your own phase.

## What M1 is, and what it is not

M1 delivers the registry on the server:
- the plain template syntax (D-V-3) in `cetra-eval`, with the shared cases in `fixtures/prompts/render.json` that the SDKs (M2) also run;
- one migration with every V table (`prompts`, `prompt_versions`, `prompt_labels`, `prompt_label_events`, `prompt_settings`);
- the store `crate::meta::prompts`, validation in `crate::prompts`, `ProjectPerm::ManagePrompts`;
- every route of the design's § API except the metrics route (M3) and the playground (M6), with OpenAPI, `fixtures/api` examples, spec text, a changelog line.

M1 does **not** add the SDK (M2), span columns or metrics (M3), any web page (M4), or the playground (M5–M7). Q1 and Q2 of the design stay out.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-V-2 | Project-scoped prompts, immutable names `[A-Za-z0-9][A-Za-z0-9_.-]*` (1–200), path key = name; 5 000 prompts per project, 10 000 versions per prompt |
| D-V-3 | `{{ name }}` only, strict; `{%`, `{#` and any other `{{…}}` refused on save |
| D-V-5 | `text` or `chat`; chat entries `{role, content}` (system, user, assistant) or `{type: "placeholder", name}`; 1–200 entries; 256 KiB; ≤ 100 variables + placeholders |
| D-V-6 | Config: JSON object ≤ 64 KiB; `model`, `connection_id`, `params`, `tools`, `tool_choice`, `response_format` validated, other keys kept |
| D-V-7 | Versions 1, 2, … immutable; commit message ≤ 500 characters |
| D-V-8 | Labels `[a-z0-9][a-z0-9_.-]{0,63}`, not all digits, not `latest`; `latest` computed; history in `prompt_label_events`; ≤ 50 labels per prompt |
| D-V-9 | Fetch defaults to `production`; missing → `404 not_found`; the API never adds a label |
| D-V-10 | Protected labels: per-project list in `prompt_settings`, default `["production"]`, ≤ 20; touching one needs a project admin or an `sk` key with `write`; else `403 forbidden` |
| D-V-12 | `GET …/prompts/{prompt}/resolve` with a strong ETag and `304` |
| D-V-13 | `View` for reads; new `ProjectPerm::ManagePrompts` (member, key scope `write`, read-write PATs) |

No user question is open for M1.

## Versions

No new dependency. `sha2` and `hex` are already dependencies of `cetra-server` (`crates/server/Cargo.toml`). `cetra-eval` gains no dependency.

## Phase graph

All phases run in order, in one worktree (they share `crates/server` files).

| Phase | Touches | Needs |
|---|---|---|
| V1.1 | `crates/eval/src/plain_template.rs`, `crates/eval/src/lib.rs`, `fixtures/prompts/` | — |
| V1.2 | migration, `meta/prompts.rs`, `meta/mod.rs`, schema snapshot | — (can be done before V1.1, but run in order) |
| V1.3 | `crate::prompts`, `auth/roles.rs`, `api/prompts.rs`, `web/src/lib/roles.ts` | V1.1, V1.2 |
| V1.4 | `routes/prompts.rs`, access, OpenAPI, examples (prompt, version, resolve routes) | V1.3 |
| V1.5 | label, label-event and settings routes, OpenAPI, examples, specs, changelog, roadmap | V1.4 |

## Conventions for every phase

- **Time**: `i64` nanoseconds in Rust and `meta.db`; JSON times use `#[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]`.
- **IDs**: `Uuid::now_v7()` for new rows, stored as hyphenated lowercase `TEXT`.
- **Actors**: `created_by`, `updated_by` and `actor` columns hold the text that `crate::routes::datasets::created_by(&principal)` returns (a user id, or `key:<key id>`, or `None`). Import it: `use super::datasets::created_by;`.
- **JSON columns**: written with `serde_json::to_string`; read with `serde_json::from_str`, a failure being `MetaError::Corrupt { table, column }`.
- **Errors**: `crate::http::error::ApiError`. Field errors are `crate::api::common::FieldError { field, message, position }`; a list of them is `ApiError::BadRequest { detail: None, errors }`. `MetaError::Conflict(&'static str)` becomes `409 conflict` through `?`. Missing things are `ApiError::NotFound`. A refused protected label is `ApiError::Forbidden { code: ProblemCode::Forbidden }`. **No new `ProblemCode`.**
- **Privacy** (AGENTS.md § Privacy): logs carry prompt, project, user and key ids, version numbers and counts, never a prompt name, label name, tag, description, template, config or commit message. Audit lines: `tracing::info!(target: "audit", prompt_id = %id, project_id = %project, version, actor = %actor.as_deref().unwrap_or("-"), "<event>")`. Every `Debug` of a type holding template or config text prints sizes only.
- **No `unwrap`/`expect`** outside tests unless a comment next to it states the invariant.
- **Tests** sit in the module they test (`#[cfg(test)] mod tests`). HTTP tests use `TestApp` (`crate::test_support`) with `#[tokio::test(flavor = "multi_thread")]`: `let app = TestApp::new().await; let seeded = app.seed().await;` (`seeded.cookie` is the org owner Ada, `seeded.read_key` a `read` key, `seeded.ingest_key` an ingest-only key), `app.write_key(&seeded)` (a `read, write` key), `app.add_member(&seeded, "m@example.com", OrgRole::Member)` (returns `Member { id, cookie }`), `app.personal_token(user_id, write)`, `capture_logs(async { … }).await` (returns `(T, String)`).
- **Machine load**: other sessions build on this machine. Do not run `cargo build/test/clippy` while iterating; compile and test **once, right before the commit**, with the focused commands of your phase, then `just check`. For the web, run only touched tests (`pnpm --dir web exec vitest run <paths> --maxWorkers=8`) while iterating. Never kill a process you did not start.
- **Commands**: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before `just check` (the web check needs Node). Focused runs: `cargo test -p cetra-eval plain_template`, `cargo test -p cetra-server --lib meta::prompts`, `cargo test -p cetra-server --lib prompts`, `cargo test -p cetra-server --test openapi`, `cargo test -p cetra-server --test api_examples`.

## Final layout after M1

```
crates/eval/src/plain_template.rs                  scan, variables, render, PlainTemplateError     (V1.1)
fixtures/prompts/render.json, README.md            shared rendering cases                          (V1.1)
crates/server/migrations/meta/0023_prompts.sql     every V table                                   (V1.2)
crates/server/src/meta/prompts.rs                  store                                           (V1.2)
crates/server/src/prompts/mod.rs                   validation: names, labels, tags, template, config (V1.3)
crates/server/src/api/prompts.rs                   API types                                       (V1.3)
crates/server/src/auth/roles.rs                    + ProjectPerm::ManagePrompts                    (V1.3)
crates/server/src/routes/prompts.rs                routes (V1.4, V1.5)
crates/server/src/api/openapi.rs                   + prompt_paths, tag "prompts"                  (V1.4, V1.5)
fixtures/api/…                                     generated and static examples                   (V1.4, V1.5)
```

## Shared reference

### Fixed identifiers (examples)

Check the block is free before V1.4: `grep -rn "8000-0000000010" fixtures crates web sdks` must print nothing, else STOP.

| Thing | Value |
|---|---|
| Prompt `support-reply` (chat) | `01900000-0000-7000-8000-000000001001` |
| Prompt `summarize` (text) | `01900000-0000-7000-8000-000000001002` |

### The example prompts

Written by `TestApp::seed_example_prompts` (V1.4) through the store, with `now` values `1_767_225_600_000_000_000` (2026-01-01T00:00:00Z) plus 1 s per write in this order, actor `None`:

1. `summarize`, text, description `Summarize a support conversation`, tags `["support"]`, version 1: template `"Summarize this conversation in {{ max_words }} words:\n\n{{ transcript }}"`, config `{"model":"gpt-4o-mini","params":{"temperature":0.2}}`, commit message `First version`, labels `["production"]`.
2. `support-reply`, chat, no description, tags `[]`, version 1: template
   `[{"role":"system","content":"You are a support agent for {{ product }}."},{"type":"placeholder","name":"history"},{"role":"user","content":"{{ question }}"}]`,
   config `{"model":"gpt-4o","params":{"temperature":0.3,"max_tokens":512}}`, commit message `First version`, labels `["production"]`.
3. `support-reply` version 2: the same template with the system content `You are a friendly support agent for {{ product }}. Answer in {{ language }}.`, config `{"model":"gpt-4o","params":{"temperature":0.3,"max_tokens":512}}`, commit message `Friendlier tone`, labels `["staging"]`.

After the seed: `support-reply` has latest version 2, labels `production → 1`, `staging → 2`, and 2 label events (`production` 1, `staging` 2). M2's and M4's fixtures read these.

### Interfaces for later milestones

Keep these names exactly; a later milestone that needs a change says so in its own plan.

| Name | Where | Used by |
|---|---|---|
| `plain_template::{scan, variables, render, Segment, PlainTemplateError}` | `cetra-eval` | M6 renders playground messages |
| `meta::prompts::{get_prompt, get_version, resolve_label, PromptRow, VersionRow}` | server | M3 metrics, M6 sources |
| `prompts::{PromptBody, parse_body, check_config, PromptConfig}` | server | M6 save-as-version path is the same API |
| `ProjectPerm::ManagePrompts` | server, `web/src/lib/roles.ts` `canManagePrompts` | M4 |
| `TestApp::seed_example_prompts`, `EXAMPLE_PROMPT = "support-reply"` | `test_support`, `api_examples.rs` | M2, M3, M4 fixtures |
| tag `prompts`, `fn prompt_paths` | `api::openapi` | M3 adds the metrics operation there |

---

## V1.1 — Plain templates in `cetra-eval` and the shared cases

**Commit subject:** `eval: add plain {{var}} prompt templates and the shared rendering cases`

### Read first

- `crates/eval/src/lib.rs` (module list and `pub use` lines), `crates/eval/src/template.rs` (house style, the `assert_no_payload` test idea), `crates/eval/src/error.rs` `TemplateError`.

### Edit

| File | Change |
|---|---|
| `crates/eval/src/plain_template.rs` | new |
| `crates/eval/src/lib.rs` | one line, `pub mod plain_template;` (the function names are generic, so the module is public instead of re-exported) |
| `fixtures/prompts/render.json` | new |
| `fixtures/prompts/README.md` | new |

Place `pub mod plain_template;` alphabetically among the `mod` lines of `lib.rs`.

### 1. Syntax (D-V-3), exactly

A template is a Rust `&str`. Scanning goes left to right over the source:

1. At `{%` or `{#`: error `Invalid { at, reason: Reason::Block }`, where `at` is the position of the `{`.
2. At `{{`: find the next `}}` after it. None → `Invalid { at, reason: Reason::Unclosed }`. Otherwise the *inner* text is the text between them. Trim spaces (U+0020) and tabs (U+0009) at both ends, nothing else. The trimmed inner must match `[A-Za-z_][A-Za-z0-9_]*` (ASCII), else `Invalid { at, reason: Reason::NotAName }`. A name in `RESERVED` is `Invalid { at, reason: Reason::Reserved }`. Otherwise it is a variable; scanning resumes after the `}}`.
3. Any other character is text (a lone `{`, a lone `}`, `}}` outside a tag are text).

`at` is a `TextAt { line: u32, column: u32 }`, both 1-based, counted in Unicode scalar values (`char`s); a `\n` ends a line (the character after it is column 1 of the next line). `\r` is an ordinary character.

```rust
/// Names minijinja reads as literals or globals; a variable cannot use them (D-V-3).
pub const RESERVED: &[&str] = &[
    "true", "false", "none", "True", "False", "None",
    "and", "or", "not", "in", "is", "if", "else",
    "loop", "self", "range", "dict", "namespace", "debug",
];
```

### 2. Types and functions (exactly these)

```rust
//! Plain `{{ name }}` prompt templates (D-V-3): substitution only, no logic.
//!
//! Every template this module accepts is also a valid minijinja template that renders the same
//! output for string values, so a registry prompt can serve as a judge template (D163), with two
//! known differences: minijinja drops one trailing newline of the template (its default
//! `keep_trailing_newline = false`), and it reports the first missing variable in sorted order
//! where this module reports the first in template order. Judges also cap templates at 64 KiB,
//! the registry at 256 KiB. The Python and TypeScript SDKs implement the same rules;
//! `fixtures/prompts/render.json` holds the shared cases.

use std::collections::{BTreeMap, BTreeSet};

/// A 1-based position in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextAt {
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
}

/// Why a template was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `{%` or `{#`.
    Block,
    /// `{{` without a later `}}`.
    Unclosed,
    /// A tag that is not one variable name.
    NotAName,
    /// A name in [`RESERVED`].
    Reserved,
}

impl Reason {
    /// The fixture and API spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Unclosed => "unclosed",
            Self::NotAName => "not_a_name",
            Self::Reserved => "reserved",
        }
    }

    /// A message for an API field error. It never quotes the template.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Block => "{% and {# are not supported; only {{ name }} is",
            Self::Unclosed => "{{ has no closing }}",
            Self::NotAName => "a tag must hold one name: letters, digits and _, not starting with a digit",
            Self::Reserved => "this variable name is reserved",
        }
    }
}

/// A template error. Neither variant holds template text, so it is safe to log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlainTemplateError {
    /// The template does not follow the syntax.
    #[error("invalid template at line {}, column {}: {}", at.line, at.column, reason.as_str())]
    Invalid {
        /// Where the offending tag starts.
        at: TextAt,
        /// Why.
        reason: Reason,
    },
    /// A variable had no value at render time.
    #[error("missing variable {name}")]
    MissingVariable {
        /// The first missing name, in template order.
        name: String,
    },
}

/// One piece of a scanned template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    /// Literal text, copied as is.
    Text(&'a str),
    /// A variable name.
    Var(&'a str),
}

/// Splits `source` into text and variables. Adjacent text is one segment; empty text segments
/// are not produced.
pub fn scan(source: &str) -> Result<Vec<Segment<'_>>, PlainTemplateError>;

/// The variable names, in order of first appearance, without duplicates.
pub fn variables(source: &str) -> Result<Vec<String>, PlainTemplateError>;

/// Renders `source`: each variable is replaced by its value, inserted verbatim (never scanned
/// again). Values not used by the template are ignored.
pub fn render(source: &str, values: &BTreeMap<String, String>) -> Result<String, PlainTemplateError>;
```

`variables` returns `Vec<String>` (order of first appearance) because the UI shows them in template order; `BTreeSet` is imported only if you need it (drop the import otherwise). `render` checks every variable before writing anything: the first variable (template order) missing from `values` is `MissingVariable`. An `Invalid` error wins over `MissingVariable` (scan first).

Implementation notes: walk with `char_indices`, tracking `line`/`column` (start at 1/1; after a `\n` set `line += 1`, `column = 1`; otherwise `column += 1`). Detect `{{`, `{%`, `{#` by looking at the next char. For the `}}` search use `source[start + 2..].find("}}")` (byte offsets are fine for slicing; positions reported are the char-based `TextAt` of the opening `{`).

### 3. `fixtures/prompts/render.json`

A JSON object `{"text": [...], "chat": [...]}`. Write it exactly with these cases (keys in this order). `text` cases are run by Rust (this phase) and both SDKs (M2). `chat` cases are run by the SDKs only (M2); write them now so the file is complete.

Text case shape: `{"name", "template", "values", "expected"}` or, for an error, `{"name", "template", "values", "error": {"kind": "invalid", "reason", "line", "column"}}` or `{"error": {"kind": "missing_variable", "name"}}`. Each case also has `"variables"`: the expected `variables()` result, or `null` when the template is invalid.

| name | template (JSON string) | values | result | variables |
|---|---|---|---|---|
| `plain-text` | `"Hello"` | `{}` | `"Hello"` | `[]` |
| `one-variable` | `"Hello {{name}}"` | `{"name":"Ada"}` | `"Hello Ada"` | `["name"]` |
| `spaces-and-tabs` | `"Hi {{ name\t}}!"` | `{"name":"Ada"}` | `"Hi Ada!"` | `["name"]` |
| `repeated` | `"{{a}}-{{b}}-{{a}}"` | `{"a":"1","b":"2"}` | `"1-2-1"` | `["a","b"]` |
| `order-of-first-use` | `"{{b}} {{a}} {{b}}"` | `{"a":"x","b":"y"}` | `"y x y"` | `["b","a"]` |
| `extra-values-ignored` | `"{{a}}"` | `{"a":"1","z":"2"}` | `"1"` | `["a"]` |
| `value-not-rescanned` | `"{{a}}"` | `{"a":"{{b}}"}` | `"{{b}}"` | `["a"]` |
| `unicode` | `"Grüße {{ name }} 👋"` | `{"name":"Zoë"}` | `"Grüße Zoë 👋"` | `["name"]` |
| `lone-braces` | `"{ a } } {"` | `{}` | `"{ a } } {"` | `[]` |
| `closing-without-opening` | `"a }} b"` | `{}` | `"a }} b"` | `[]` |
| `underscore-name` | `"{{_x1}}"` | `{"_x1":"v"}` | `"v"` | `["_x1"]` |
| `empty-value` | `"[{{a}}]"` | `{"a":""}` | `"[]"` | `["a"]` |
| `multiline-value` | `"{{a}}"` | `{"a":"l1\nl2"}` | `"l1\nl2"` | `["a"]` |
| `missing` | `"{{a}} {{b}}"` | `{"a":"1"}` | missing `b` | `["a","b"]` |
| `missing-first-in-order` | `"{{b}} {{a}}"` | `{}` | missing `b` | `["b","a"]` |
| `block` | `"x {% if a %}"` | `{}` | invalid `block` 1:3 | `null` |
| `comment` | `"{# note #}"` | `{}` | invalid `block` 1:1 | `null` |
| `unclosed` | `"Hi {{ name"` | `{}` | invalid `unclosed` 1:4 | `null` |
| `filter` | `"{{ name \| upper }}"` | `{}` | invalid `not_a_name` 1:1 | `null` |
| `dotted` | `"{{ user.name }}"` | `{}` | invalid `not_a_name` 1:1 | `null` |
| `digit-first` | `"{{1a}}"` | `{}` | invalid `not_a_name` 1:1 | `null` |
| `empty-tag` | `"{{ }}"` | `{}` | invalid `not_a_name` 1:1 | `null` |
| `whitespace-control` | `"{{- a }}"` | `{}` | invalid `not_a_name` 1:1 | `null` |
| `newline-in-tag` | `"{{ a\n}}"` | `{}` | invalid `not_a_name` 1:1 | `null` |
| `reserved` | `"{{ true }}"` | `{}` | invalid `reserved` 1:1 | `null` |
| `reserved-not` | `"{{ not }}"` | `{}` | invalid `reserved` 1:1 | `null` |
| `reserved-if` | `"x {{if}}"` | `{}` | invalid `reserved` 1:3 | `null` |
| `trailing-newline` | `"{{a}}\n"` | `{"a":"x"}` | `"x\n"` | `["a"]` |
| `position-second-line` | `"ok\nab {{ x.y }}"` | `{}` | invalid `not_a_name` 2:4 | `null` |
| `position-after-emoji` | `"👋 {%"` | `{}` | invalid `block` 1:3 | `null` |

(In the `filter` row the template is `{{ name | upper }}`; the `\|` is only Markdown escaping.)

Chat case shape: `{"name", "template": [entries], "values", "placeholders", "expected": [messages]}` or `{"error": {"kind": "missing_variable", "name"}}` / `{"error": {"kind": "missing_placeholder", "name"}}`, plus `"variables"` and `"placeholder_names"` (order of first appearance across entries). Rules (D-V-5): each message's `content` renders as a text template; a placeholder is replaced, in place, by the given list of messages, copied unchanged (their content is not rendered); missing variables are checked across all messages in order before placeholders; then the first missing placeholder in order is `missing_placeholder`.

| name | template | values | placeholders | result |
|---|---|---|---|---|
| `chat-basic` | `[{"role":"system","content":"You help with {{product}}."},{"role":"user","content":"{{q}}"}]` | `{"product":"cetra","q":"Hi"}` | `{}` | `[{"role":"system","content":"You help with cetra."},{"role":"user","content":"Hi"}]`; variables `["product","q"]`, placeholder_names `[]` |
| `chat-placeholder` | `[{"role":"system","content":"S"},{"type":"placeholder","name":"history"},{"role":"user","content":"{{q}}"}]` | `{"q":"Next"}` | `{"history":[{"role":"user","content":"{{not_rendered}}"},{"role":"assistant","content":"A"}]}` | `[{"role":"system","content":"S"},{"role":"user","content":"{{not_rendered}}"},{"role":"assistant","content":"A"},{"role":"user","content":"Next"}]`; variables `["q"]`, placeholder_names `["history"]` |
| `chat-empty-placeholder` | `[{"type":"placeholder","name":"h"},{"role":"user","content":"x"}]` | `{}` | `{"h":[]}` | `[{"role":"user","content":"x"}]`; variables `[]`, placeholder_names `["h"]` |
| `chat-missing-variable` | `[{"role":"user","content":"{{a}}"},{"type":"placeholder","name":"h"}]` | `{}` | `{}` | missing_variable `a` |
| `chat-missing-placeholder` | `[{"role":"user","content":"x"},{"type":"placeholder","name":"h"}]` | `{}` | `{}` | missing_placeholder `h`; variables `[]`, placeholder_names `["h"]` |

Error cases still carry `variables` and `placeholder_names`.

`fixtures/prompts/README.md`: three short paragraphs: what the file is (the shared cases of D-V-3 and D-V-5), who runs it (`cetra-eval`'s `plain_template` tests for `text`; the Python and TypeScript SDK tests for both), and that positions are 1-based lines and columns in Unicode scalar values (a JavaScript implementation must count code points, not UTF-16 units).

### Tests (`plain_template.rs`, `#[cfg(test)] mod tests`)

1. `shared_text_cases` — reads `include_str!("../../../fixtures/prompts/render.json")` (relative to `crates/eval/src/plain_template.rs`), and for every `text` case: `variables(template)` equals `variables` (or is an `Invalid` error when it is `null`); `render(template, values)` equals `expected`, or the error equals the case's error (`Invalid { at: TextAt { line, column }, reason }` with `reason.as_str() == reason`, or `MissingVariable { name }`). Every case name appears in the assertion message.
2. `scan_segments` — `scan("a{{x}}b{{ y }}")` is `[Text("a"), Var("x"), Text("b"), Var("y")]`; `scan("")` is `[]`; `scan("{{x}}{{y}}")` is `[Var("x"), Var("y")]`.
3. `accepted_templates_render_the_same_in_minijinja` — for every valid text case (no `error`), build a `crate::PromptTemplate::compile(template)` and render it with `crate::Variables::from_map(values as BTreeMap<String, serde_json::Value::String>)`; the output equals `expected`, except for the case `trailing-newline`, where it equals `expected` without its final `\n` (minijinja's default; module doc). For every `invalid` case whose reason is `reserved`, assert that `PromptTemplate::compile` fails, or compiles and renders without any value supplied (it names an engine literal or global). This proves the subset property (D-V-3). If any other case differs, STOP and report the case name.
4. `errors_hold_no_template_text` — render a template `"{{ secret_payload.x }}"` and `"{{secret_payload}}"` with no values; the `Display` of both errors does not contain `secret_payload` for the `Invalid` one (the name is allowed in `MissingVariable`, which only holds the variable name; assert the Invalid display has no `secret_payload`).
5. `proptest_render_never_panics` — with `proptest` (a dev-dependency of `cetra-eval`) and the crate's config `crate::hostile_tests::config()` (from `crates/eval/tests/hostile.rs`, included by `lib.rs`; it reads `CETRA_PROPTEST_CASES` and turns failure persistence off), in `proptest! { #![proptest_config(crate::hostile_tests::config())] … }`: for any `String` source, `scan`, `variables` and `render(&source, &BTreeMap::new())` return without panicking; and for a source built as text without `{`/`}` plus `{{v0}}`, `render` with `v0 → x` equals the text with `x` in place.

### Done when

- `cargo test -p cetra-eval plain_template` passes, then `just check` is green.

---

## V1.2 — Migration and the `meta::prompts` store

**Commit subject:** `server: add the prompt registry tables and store`

### Read first

- `crates/server/src/meta/mod.rs`: `MetaDb::write`, `reader`, `MetaError`, `unique_or`, `test_db`, the module list, the test `schema_matches_the_snapshot`.
- `crates/server/src/meta/datasets.rs`: the row/record pattern, `corrupt`, `parse_uuid`, `insert_dataset` (cap then insert then read back), `write_version`, `list_versions`, the tests' `org`/`project` helpers and `NOW`.
- `crates/server/migrations/meta/0017_datasets.sql` (SQL style).

### Edit

| File | Change |
|---|---|
| `crates/server/migrations/meta/0023_prompts.sql` | new (if `0023` is taken by the time you start, use the next free number and say so) |
| `crates/server/src/meta/prompts.rs` | new |
| `crates/server/src/meta/mod.rs` | `pub(crate) mod prompts;` (alphabetical) |
| `crates/server/tests/snapshots/meta_schema.snap` | regenerated |

### 1. Migration, exactly

```sql
-- Track V: the prompt registry (D-V-7, D-V-8, D-V-10, D-V-11, spec 04 § Prompts).
CREATE TABLE prompts (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,                   -- immutable (D-V-2)
  type TEXT NOT NULL CHECK (type IN ('text', 'chat')),
  description TEXT,
  tags TEXT NOT NULL DEFAULT '[]',      -- JSON array of strings
  latest_version INTEGER NOT NULL,
  created_by TEXT,                      -- a user id or key:<key id>
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);

CREATE TABLE prompt_versions (
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  template TEXT NOT NULL,               -- JSON: a string (text) or an array (chat)
  config TEXT NOT NULL DEFAULT '{}',    -- JSON object (D-V-6)
  commit_message TEXT,
  created_by TEXT,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (prompt_id, version)
);

CREATE TABLE prompt_labels (
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  label TEXT NOT NULL,
  version INTEGER NOT NULL,
  updated_by TEXT,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (prompt_id, label),
  FOREIGN KEY (prompt_id, version) REFERENCES prompt_versions(prompt_id, version)
);

CREATE TABLE prompt_label_events (
  id INTEGER PRIMARY KEY,               -- rowid: the history's order
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  label TEXT NOT NULL,
  from_version INTEGER,                 -- NULL: the label was new
  to_version INTEGER,                   -- NULL: the label was removed
  actor TEXT,
  at INTEGER NOT NULL
);
CREATE INDEX prompt_label_events_prompt ON prompt_label_events(prompt_id, id);

CREATE TABLE prompt_settings (
  project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  protected_labels TEXT NOT NULL,       -- JSON array; no row means ["production"] (D-V-10)
  updated_by TEXT,
  updated_at INTEGER NOT NULL
);
```

Regenerate the snapshot: `INSTA_UPDATE=always cargo test -p cetra-server --lib schema_matches_the_snapshot`. `git diff crates/server/tests/snapshots/meta_schema.snap` must show only added blocks for these five tables, the index `prompt_label_events_prompt`, and SQLite's automatic indexes (`sqlite_autoindex_prompts_1`, `…_2`, `sqlite_autoindex_prompt_versions_1`, `sqlite_autoindex_prompt_labels_1`, `sqlite_autoindex_prompt_settings_1`, or the names SQLite picks). Anything else: STOP.

### 2. `meta/prompts.rs`

Module doc: `//! The prompt registry's rows (spec 04 § Prompts, D-V-7, D-V-8, D-V-10, D-V-11).`

Imports: `std::fmt`, `cetra_core::ids::ProjectId`, `sqlx::{AssertSqlSafe, SqliteConnection, SqlitePool}` (only what you use), `uuid::Uuid`, `super::{MetaError, unique_or}`.

#### Constants

```rust
/// A project holds at most this many prompts (D-V-2).
pub(crate) const PROJECT_CAP: i64 = 5_000;
/// A prompt has at most this many versions (D-V-2).
pub(crate) const VERSION_CAP: i64 = 10_000;
/// A prompt carries at most this many labels (D-V-8).
pub(crate) const LABEL_CAP: i64 = 50;
/// The protected labels of a project without a `prompt_settings` row (D-V-10).
pub(crate) const DEFAULT_PROTECTED: &[&str] = &["production"];
/// Conflict reasons.
pub(crate) const DUPLICATE_NAME: &str = "a prompt with this name already exists";
pub(crate) const TOO_MANY_PROMPTS: &str = "a project has at most 5000 prompts";
pub(crate) const TOO_MANY_VERSIONS: &str = "a prompt has at most 10000 versions";
pub(crate) const TOO_MANY_LABELS: &str = "a prompt has at most 50 labels";
```

#### Types (exactly these)

```rust
/// `prompts.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptType {
    Text,
    Chat,
}

impl PromptType {
    /// The stored and API spelling: `text` or `chat`.
    pub(crate) const fn as_str(self) -> &'static str;
    /// Parses the stored spelling. Anything else is `None`.
    pub(crate) fn parse(text: &str) -> Option<Self>;
}

/// One label on a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelRow {
    pub(crate) label: String,
    pub(crate) version: i64,
    pub(crate) updated_by: Option<String>,
    pub(crate) updated_at: i64,
}

/// One stored prompt with its labels (ordered by label).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PromptRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) name: String,
    pub(crate) prompt_type: PromptType,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) latest_version: i64,
    pub(crate) labels: Vec<LabelRow>,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// One version. `template` and `config` are the stored JSON texts. `Debug` prints sizes only.
#[derive(Clone, PartialEq)]
pub(crate) struct VersionRow {
    pub(crate) prompt_id: Uuid,
    pub(crate) version: i64,
    pub(crate) template: String,
    pub(crate) config: String,
    pub(crate) commit_message: Option<String>,
    /// Labels pointing at this version, ordered.
    pub(crate) labels: Vec<String>,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
}

/// A prompt to insert.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewPrompt {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) prompt_type: PromptType,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
}

/// A version to append, and the labels to point at it. `Debug` prints sizes only.
#[derive(Clone, PartialEq)]
pub(crate) struct NewVersion {
    pub(crate) template: String,
    pub(crate) config: String,
    pub(crate) commit_message: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) actor: Option<String>,
}

/// One label history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelEventRow {
    pub(crate) id: i64,
    pub(crate) label: String,
    pub(crate) from_version: Option<i64>,
    pub(crate) to_version: Option<i64>,
    pub(crate) actor: Option<String>,
    pub(crate) at: i64,
}

/// What [`set_label`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LabelChange {
    /// The label was new or moved; `from` is its previous version.
    Moved { from: Option<i64> },
    /// The label already pointed at that version: nothing written.
    Unchanged,
}

/// List filters. `q` matches a case-insensitive substring of the name (LIKE, escaped as
/// `meta::datasets` does with `escape_like`); `label` keeps prompts carrying that label;
/// `tag` keeps prompts whose tags contain it exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PromptFilter {
    pub(crate) q: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) tag: Option<String>,
}
```

Hand-written `Debug` for `VersionRow` (`prompt_id`, `version`, `template_bytes`, `config_bytes`, `label_count`) and `NewVersion` (`template_bytes`, `config_bytes`, `label_count`), both with `finish_non_exhaustive()`. If `meta::datasets` has `escape_like` as a private function, copy its 5-line body into this module as a private `fn escape_like` (do not change `datasets.rs`); if it is `pub(crate)`, import it.

#### Functions (exactly these signatures)

```rust
/// Every prompt of `project` matching `filter`, ordered by `lower(name)`, then `id`.
pub(crate) async fn list_prompts(pool: &SqlitePool, project: ProjectId, filter: &PromptFilter)
    -> Result<Vec<PromptRow>, MetaError>;

/// The prompt of `project` named `name`.
pub(crate) async fn get_prompt(pool: &SqlitePool, project: ProjectId, name: &str)
    -> Result<Option<PromptRow>, MetaError>;

/// The same read inside a write transaction.
pub(crate) async fn get_prompt_tx(conn: &mut SqliteConnection, project: ProjectId, name: &str)
    -> Result<Option<PromptRow>, MetaError>;

/// Inserts `prompt` with `first` as version 1 and its labels. Past [`PROJECT_CAP`] is
/// `Conflict(TOO_MANY_PROMPTS)`; a duplicate name is `Conflict(DUPLICATE_NAME)`.
pub(crate) async fn create_prompt(conn: &mut SqliteConnection, project: ProjectId,
    prompt: NewPrompt, first: NewVersion, now: i64) -> Result<(PromptRow, VersionRow), MetaError>;

/// Appends version `latest_version + 1` of `prompt_id` and points its labels at it. Past
/// [`VERSION_CAP`] is `Conflict(TOO_MANY_VERSIONS)`.
pub(crate) async fn add_version(conn: &mut SqliteConnection, prompt_id: Uuid,
    new: NewVersion, now: i64) -> Result<VersionRow, MetaError>;

/// Replaces `description` when `Some` (the inner `None` clears it) and `tags` when `Some`,
/// and sets `updated_at`. `None` when no such prompt in `project`.
pub(crate) async fn update_prompt(conn: &mut SqliteConnection, project: ProjectId, name: &str,
    description: Option<Option<String>>, tags: Option<Vec<String>>, now: i64)
    -> Result<Option<PromptRow>, MetaError>;

/// Deletes the prompt and, by cascade, its versions, labels and history. False when absent.
pub(crate) async fn delete_prompt(conn: &mut SqliteConnection, project: ProjectId, id: Uuid)
    -> Result<bool, MetaError>;

/// One version.
pub(crate) async fn get_version(pool: &SqlitePool, prompt_id: Uuid, version: i64)
    -> Result<Option<VersionRow>, MetaError>;

/// The version a stored label points at (`latest` is not stored: callers use `get_version`).
pub(crate) async fn resolve_label(pool: &SqlitePool, prompt_id: Uuid, label: &str)
    -> Result<Option<VersionRow>, MetaError>;

/// Versions newest first, strictly below `before` when set; at most `limit + 1` rows.
pub(crate) async fn list_versions(pool: &SqlitePool, prompt_id: Uuid, before: Option<i64>,
    limit: u32) -> Result<Vec<VersionRow>, MetaError>;

/// Points `label` at `version` and records the move. A version that does not exist is
/// `Ok(None)`. A new label past [`LABEL_CAP`] is `Conflict(TOO_MANY_LABELS)`.
pub(crate) async fn set_label(conn: &mut SqliteConnection, prompt_id: Uuid, label: &str,
    version: i64, actor: Option<&str>, now: i64) -> Result<Option<LabelChange>, MetaError>;

/// Removes `label` and records it. False when the label is not set.
pub(crate) async fn remove_label(conn: &mut SqliteConnection, prompt_id: Uuid, label: &str,
    actor: Option<&str>, now: i64) -> Result<bool, MetaError>;

/// History newest first, with `id < before` when set; at most `limit + 1` rows.
pub(crate) async fn list_label_events(pool: &SqlitePool, prompt_id: Uuid, before: Option<i64>,
    limit: u32) -> Result<Vec<LabelEventRow>, MetaError>;

/// The project's protected labels: its `prompt_settings` row, else [`DEFAULT_PROTECTED`].
pub(crate) async fn protected_labels(pool: &SqlitePool, project: ProjectId)
    -> Result<Vec<String>, MetaError>;

/// The same inside a write transaction.
pub(crate) async fn protected_labels_tx(conn: &mut SqliteConnection, project: ProjectId)
    -> Result<Vec<String>, MetaError>;

/// Upserts the project's protected labels.
pub(crate) async fn set_protected_labels(conn: &mut SqliteConnection, project: ProjectId,
    labels: &[String], actor: Option<&str>, now: i64) -> Result<(), MetaError>;
```

#### SQL and rules

- **Prompt reads** (`list_prompts`, `get_prompt`, `get_prompt_tx`): select the prompt columns from `prompts`, then load its labels with `SELECT label, version, updated_by, updated_at FROM prompt_labels WHERE prompt_id = ? ORDER BY label`. For `list_prompts`, load all labels of the listed prompts in one query (`WHERE prompt_id IN (SELECT id FROM prompts WHERE project_id = ?)` ordered by `prompt_id, label`) and attach them in memory; do not query per prompt.
  - `list_prompts` SQL: `SELECT … FROM prompts p WHERE p.project_id = ?1 AND (?2 IS NULL OR lower(p.name) LIKE '%' || lower(?2) || '%' ESCAPE '\') AND (?3 IS NULL OR EXISTS (SELECT 1 FROM prompt_labels l WHERE l.prompt_id = p.id AND l.label = ?3)) AND (?4 IS NULL OR EXISTS (SELECT 1 FROM json_each(p.tags) t WHERE t.value = ?4)) ORDER BY lower(p.name), p.id`, binding the escaped `q`. A `label` filter of `latest` matches every prompt: the route passes `None` for it (V1.4).
  - `tags` decodes as `Vec<String>`; failure → `Corrupt { table: "prompts", column: "tags" }`. `type` → `PromptType::parse`, `None` → `Corrupt { table: "prompts", column: "type" }`.
- **`create_prompt`**, in order on `conn`:
  1. `SELECT COUNT(*) FROM prompts WHERE project_id = ?`; `>= PROJECT_CAP` → `Conflict(TOO_MANY_PROMPTS)`.
  2. `INSERT INTO prompts (id, project_id, name, type, description, tags, latest_version, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?, ?)` (`created_by` = `first.actor`), `.map_err(|e| unique_or(e, DUPLICATE_NAME))`.
  3. `add_version(conn, id, first, now)`.
  4. Read back with `get_prompt_tx` (`None` → `Corrupt { table: "prompts", column: "id" }`). Return `(prompt, version)`.
- **`add_version`**, in order:
  1. `SELECT latest_version FROM prompts WHERE id = ?` (`None` → `Corrupt { table: "prompts", column: "id" }`). `v = latest + 1`; `v > VERSION_CAP` → `Conflict(TOO_MANY_VERSIONS)`.
  2. `INSERT INTO prompt_versions (prompt_id, version, template, config, commit_message, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)`.
  3. `UPDATE prompts SET latest_version = ?, updated_at = ? WHERE id = ?`.
  4. For each label of `new.labels` in order: `set_label(conn, prompt_id, label, v, new.actor.as_deref(), now)`; its `Ok(None)` cannot happen (the version was just inserted): map it to `Corrupt { table: "prompt_versions", column: "version" }`.
  5. Return `get_version` of `v` read on `conn` (write a private `get_version_tx(conn, prompt_id, version)` used here and by `get_version`'s logic; both share one `const VERSION_SELECT`).
- **Version reads**: `labels` of a version = `SELECT label FROM prompt_labels WHERE prompt_id = ? AND version = ? ORDER BY label`. `list_versions` reads the page with `SELECT … FROM prompt_versions WHERE prompt_id = ?1 AND (?2 IS NULL OR version < ?2) ORDER BY version DESC LIMIT ?3` (binding `limit + 1`), then the labels of those versions in one query, attached in memory.
- **`resolve_label`**: `SELECT version FROM prompt_labels WHERE prompt_id = ? AND label = ?`, then `get_version`.
- **`set_label`**, in order:
  1. `SELECT 1 FROM prompt_versions WHERE prompt_id = ? AND version = ?`; none → `Ok(None)`.
  2. `SELECT version FROM prompt_labels WHERE prompt_id = ? AND label = ?` → `from`.
  3. `from == Some(version)` → `Ok(Some(LabelChange::Unchanged))`.
  4. `from == None`: `SELECT COUNT(*) FROM prompt_labels WHERE prompt_id = ?`; `>= LABEL_CAP` → `Conflict(TOO_MANY_LABELS)`.
  5. `INSERT INTO prompt_labels (prompt_id, label, version, updated_by, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(prompt_id, label) DO UPDATE SET version = excluded.version, updated_by = excluded.updated_by, updated_at = excluded.updated_at`.
  6. `INSERT INTO prompt_label_events (prompt_id, label, from_version, to_version, actor, at) VALUES (?, ?, ?, ?, ?, ?)` with `to_version = version`.
  7. `UPDATE prompts SET updated_at = ? WHERE id = ?`. Return `Ok(Some(LabelChange::Moved { from }))`.
- **`remove_label`**: read `from` as in step 2; none → `Ok(false)`. `DELETE FROM prompt_labels WHERE prompt_id = ? AND label = ?`, then the event with `from_version = from`, `to_version = NULL`, then the `updated_at` update. `Ok(true)`.
- **`list_label_events`**: `SELECT id, label, from_version, to_version, actor, at FROM prompt_label_events WHERE prompt_id = ?1 AND (?2 IS NULL OR id < ?2) ORDER BY id DESC LIMIT ?3`, binding `limit + 1`.
- **`update_prompt`**: build one `UPDATE prompts SET updated_at = ?` plus `description = ?` and/or `tags = ?` when given (`COALESCE` is wrong here because `Some(None)` must clear: write the four cases as four literal statements, or use `CASE WHEN ?flag THEN ? ELSE description END`), `WHERE project_id = ? AND name = ?`. `rows_affected() == 0` → `Ok(None)`; else `get_prompt_tx`.
- **`delete_prompt`**: `DELETE FROM prompts WHERE project_id = ? AND id = ?`; `rows_affected() > 0`. The foreign key from `prompt_labels` to `prompt_versions` is checked at the end of the statement, after the cascade removed both, so it does not block.
- **`protected_labels`/`_tx`**: `SELECT protected_labels FROM prompt_settings WHERE project_id = ?`; none → `DEFAULT_PROTECTED` as `Vec<String>`; text that does not decode as `Vec<String>` → `Corrupt { table: "prompt_settings", column: "protected_labels" }`.
- **`set_protected_labels`**: `INSERT INTO prompt_settings (project_id, protected_labels, updated_by, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(project_id) DO UPDATE SET protected_labels = excluded.protected_labels, updated_by = excluded.updated_by, updated_at = excluded.updated_at`.

Splice only constants into SQL (`format!` + `AssertSqlSafe(sql)` with the comment `// Only constants are spliced; every value is bound.`); bind every value.

### Tests (`meta/prompts.rs`)

Helpers: `org` and `project` exactly as in `meta/datasets.rs` tests; `const NOW: i64 = 1_000;`; `fn new_prompt(name: &str, prompt_type: PromptType) -> NewPrompt` (fresh id, no description, no tags); `fn version(template: &str, labels: &[&str]) -> NewVersion` (config `{}`, no message, actor `Some("user-1")`).

1. `create_get_list_and_delete` — create `beta`, `Alpha` (chat) and `gamma`; `list_prompts` with the default filter returns `Alpha`, `beta`, `gamma`; `get_prompt("beta")` has `latest_version == 1`, `prompt_type == Text`; `get_version(id, 1)` returns the template text; `delete_prompt` is true then false; `get_prompt` is `None`; `get_version` is `None` and `SELECT COUNT(*) FROM prompt_label_events` for it is 0.
2. `duplicate_name_is_a_conflict` — a second `beta` in the same project is `Conflict(DUPLICATE_NAME)`; the same name in another project is fine; the failed write left no row (count of prompts named `beta` in the project is 1).
3. `versions_count_up_and_labels_move` — create `p` with labels `["production"]`; `add_version` with `["staging"]` returns version 2 with `labels == ["staging"]`; `add_version` with `["production"]` returns 3; `resolve_label("production")` is version 3, `resolve_label("staging")` is 2; `get_version(1).labels` is empty; `get_prompt.labels` is `production → 3`, `staging → 2`; `list_label_events(None, 10)` is, newest first: production 1→3, staging none→2, production none→1.
4. `set_label_cases` — `set_label` to version 9 (absent) is `Ok(None)`; to the current version is `Unchanged` and writes no event (the event count stays the same); to another version is `Moved { from: Some(_) }`; a new label is `Moved { from: None }`.
5. `remove_label_cases` — removing a set label is true and writes an event with `to_version == None`; removing again is false.
6. `label_cap` — 50 labels `l00`…`l49` set on version 1 in one write; a 51st new label is `Conflict(TOO_MANY_LABELS)`; moving an existing label still works.
7. `project_cap` — 5 000 prompts in one write (loop inside one `db.write`); one more is `Conflict(TOO_MANY_PROMPTS)`.
8. `version_cap` — `UPDATE prompts SET latest_version = 10000` directly (sqlx query in the test), then `add_version` is `Conflict(TOO_MANY_VERSIONS)`.
9. `update_prompt_tri_state` — `Some(Some("d"))` sets, `None` keeps, `Some(None)` clears the description; `tags` replaced when `Some`; unknown name is `Ok(None)`.
10. `filters` — prompts `support-a` (tags `["x"]`, label `production`), `support-b`, `other`: `q = "SUPPORT"` returns the two support prompts; `q = "%"` returns none (escaped); `label = "production"` returns `support-a`; `tag = "x"` returns `support-a`.
11. `paging` — 5 versions; `list_versions(None, 2)` returns versions 5, 4, 3 (limit + 1); `list_versions(Some(3), 10)` returns 2, 1; the same pattern for `list_label_events`.
12. `protected_labels_default_and_set` — a new project returns `["production"]`; after `set_protected_labels(["prod", "live"])` it returns those; after `set_protected_labels([])` it returns `[]` (an explicit empty list protects nothing).
13. `debug_prints_no_content` — the `Debug` of a `VersionRow` and a `NewVersion` whose template is `"canary-debug"` contains no `canary-debug`.

### Done when

- `cargo test -p cetra-server --lib meta::prompts` and `cargo test -p cetra-server --lib schema_matches_the_snapshot` pass; `just check` is green.

---

## V1.3 — Validation, `ManagePrompts`, API types

**Commit subject:** `server: add prompt validation, ManagePrompts and the prompt API types`

### Read first

- V1.1's `crates/eval/src/plain_template.rs` and V1.2's `meta/prompts.rs`.
- `crates/server/src/datasets/mod.rs` (`check_name`, `check_description`: style of a validation module that returns `FieldError`s).
- `crates/server/src/auth/roles.rs`: `ProjectPerm`, `min_role`, `key_scope`, the test `permissions_follow_the_spec_table`.
- `crates/server/src/api/datasets.rs`: derives, the private `present` deserializer for tri-state PATCH fields, `DatasetListParams`, how times are serialized.
- `crates/server/src/api/common.rs`: `Page`, `Limit`, `FieldError`, `TextPosition`, `any_json`.
- `web/src/lib/roles.ts` and `web/src/lib/roles.test.ts` (`canManageDatasets` and its tests).

### Edit

| File | Change |
|---|---|
| `crates/server/src/prompts/mod.rs` | new |
| `crates/server/src/lib.rs` | `pub(crate) mod prompts;` (alphabetical) |
| `crates/server/src/auth/roles.rs` | `ProjectPerm::ManagePrompts`, `min_role`, `key_scope`, test asserts |
| `crates/server/src/api/prompts.rs` | new |
| `crates/server/src/api/mod.rs` | `pub mod prompts;` (alphabetical) |
| `web/src/lib/roles.ts`, `web/src/lib/roles.test.ts` | `canManagePrompts` and its test |

If the compiler reports another exhaustive `match` on `ProjectPerm` elsewhere, add the variant there with the same meaning and say so in your report.

### 1. `ProjectPerm::ManagePrompts`

After `ManageDatasets`:

```rust
    /// Prompts: create, version, label (unprotected; D-V-10 for protected ones), edit, delete
    /// (D-V-13). Member and above; a `write` API key; a read-write personal access token.
    ManagePrompts,
```

`min_role`: add to the `Role::Member` arm. `key_scope`: add to the `Some(KeyScope::Write)` arm. Test, after the `ManageDatasets` asserts:

```rust
        // Prompts: member and above, and a `write` key (D-V-13).
        assert_eq!(ProjectPerm::ManagePrompts.min_role(), Role::Member);
        assert_eq!(ProjectPerm::ManagePrompts.key_scope(), Some(KeyScope::Write));
```

`web/src/lib/roles.ts`, after `canManageDatasets`:

```ts
/** Mirrors `ProjectPerm::ManagePrompts`: members and above create and label prompts (D-V-13). */
export function canManagePrompts(role: OrgRole | undefined): boolean {
  return atLeast(role, "member");
}
```

with a test in `roles.test.ts` in the same style as `canManageDatasets`'s (viewer false, member true, admin true, owner true, undefined false).

### 2. `prompts/mod.rs` — validation (no database access)

Module doc: `//! Prompt validation: names, labels, tags, templates and configs (D-V-2 to D-V-8).`

Until V1.4 calls it, add right after the doc: `#![cfg_attr(not(test), allow(dead_code))] // V1.4 removes this line.`

```rust
use std::collections::BTreeSet;

use cetra_eval::plain_template::{self, PlainTemplateError};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::api::common::{FieldError, TextPosition};
use crate::meta::prompts::PromptType;

pub(crate) const MAX_NAME_CHARS: usize = 200;
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 2_000;
pub(crate) const MAX_COMMIT_CHARS: usize = 500;
pub(crate) const MAX_TAGS: usize = 20;
pub(crate) const MAX_TAG_CHARS: usize = 64;
pub(crate) const MAX_LABELS_PER_REQUEST: usize = 50;
pub(crate) const MAX_PROTECTED: usize = 20;
pub(crate) const MAX_TEMPLATE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CHAT_ENTRIES: usize = 200;
pub(crate) const MAX_NAMES: usize = 100;
pub(crate) const MAX_CONFIG_BYTES: usize = 64 * 1024;
pub(crate) const MAX_TOOLS: usize = 128;
/// The label the fetch resolves by default (D-V-9).
pub(crate) const DEFAULT_LABEL: &str = "production";
/// The computed label (D-V-8).
pub(crate) const LATEST: &str = "latest";

/// A chat role (D-V-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatRole { System, User, Assistant }

/// One chat entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChatEntry {
    Message { role: ChatRole, content: String },
    Placeholder { name: String },
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptBody {
    Text(String),
    Chat(Vec<ChatEntry>),
}

impl PromptBody {
    /// Variable names in order of first appearance across the body.
    pub(crate) fn variables(&self) -> Vec<String>;
    /// Placeholder names in order of first appearance (empty for text).
    pub(crate) fn placeholders(&self) -> Vec<String>;
    /// The JSON stored in `prompt_versions.template` and returned by the API.
    pub(crate) fn to_json(&self) -> Value;
}
```

`PromptBody`'s `Debug` must print sizes only: replace the derive with a hand-written `Debug` (`Text { chars }` / `Chat { entries }`), and the same for `ChatEntry` (role or `placeholder`, `chars`). `to_json`: text → `Value::String`; chat → array of `{"role": "system"|"user"|"assistant", "content": …}` or `{"type": "placeholder", "name": …}`, keys in that order.

Functions (exactly these):

```rust
/// D-V-2: 1–200 characters, `[A-Za-z0-9][A-Za-z0-9_.-]*`. Not trimmed (a space is refused).
/// Field `name`. Message: "must be 1 to 200 characters: letters, digits, '_', '.', '-', starting with a letter or digit".
pub(crate) fn check_name(name: &str) -> Result<(), FieldError>;

/// D-V-8: `[a-z0-9][a-z0-9_.-]{0,63}`, not only digits, not `latest`. Field as given.
/// Messages: "must be 1 to 64 characters: lowercase letters, digits, '_', '.', '-'",
/// "must not be only digits", "latest is computed and cannot be set".
pub(crate) fn check_label(field: &str, label: &str) -> Result<(), FieldError>;

/// Every label of a request: each checked (`<field>[i]`), at most `max`, no duplicates
/// ("must not repeat a label"), returned in request order. Field `<field>` for the count:
/// "must hold at most <max> labels".
pub(crate) fn check_labels(field: &str, labels: &[String], max: usize) -> Result<Vec<String>, Vec<FieldError>>;

/// At most 2 000 characters; empty → None. Field `description`.
pub(crate) fn check_description(text: &str) -> Result<Option<String>, FieldError>;

/// At most 500 characters; empty → None. Field `commit_message`.
pub(crate) fn check_commit_message(text: &str) -> Result<Option<String>, FieldError>;

/// Each tag trimmed, 1–64 characters (`tags[i]`, "must be 1 to 64 characters"), at most 20
/// ("must hold at most 20 tags"), duplicates dropped (first kept), order kept.
pub(crate) fn check_tags(tags: &[String]) -> Result<Vec<String>, Vec<FieldError>>;

/// Parses and checks a template of `prompt_type` (D-V-3, D-V-5). Field paths: `template` for the
/// whole value, `template[i]`, `template[i].role`, `template[i].content`, `template[i].name`.
pub(crate) fn parse_body(prompt_type: PromptType, template: &Value) -> Result<PromptBody, Vec<FieldError>>;

/// Checks a config object (D-V-6) and returns its stored text. Field paths start with `config`.
pub(crate) fn check_config(config: &Value) -> Result<String, Vec<FieldError>>;
```

**`parse_body` rules**, in order (collect every error; return them all):
1. The serialized size of `template` (`serde_json::to_string(template)?.len()`) > 256 KiB → `template`: "must be at most 262144 bytes" (stop here).
2. `Text`: `template` must be a JSON string, else `template`: "must be a string for a text prompt". Then `plain_template::variables(text)`: an `Invalid { at, reason }` → `FieldError { field: "template", message: reason.message(), position: Some(TextPosition { line: at.line, column: at.column }) }`.
3. `Chat`: `template` must be an array, else `template`: "must be an array of messages for a chat prompt"; 1 to 200 entries, else `template`: "must hold 1 to 200 messages". Each entry `i`:
   - an object; else `template[i]`: "must be an object".
   - If it has a `type` key: it must be `"placeholder"` (else `template[i].type`: "must be placeholder"), keys exactly `type` and `name` (else `template[i]`: "a placeholder has only type and name"), `name` a string matching `[A-Za-z_][A-Za-z0-9_]*` and not in `plain_template::RESERVED` (else `template[i].name`: "must be a name: letters, digits and _, not starting with a digit").
   - Otherwise keys exactly `role` and `content` (else `template[i]`: "a message has only role and content"); `role` one of `system`, `user`, `assistant` (else `template[i].role`: "must be system, user or assistant"); `content` a string (else `template[i].content`: "must be a string"), checked with `plain_template::variables` like a text template, the error on `template[i].content` with its position.
4. Then the distinct variables plus distinct placeholders > 100 → `template`: "must use at most 100 variables and placeholders". A name used both as a variable and as a placeholder → `template`: "a name cannot be both a variable and a placeholder".

**`check_config` rules** (collect every error):
- Must be a JSON object (`config`: "must be a JSON object"); serialized size ≤ 64 KiB (`config`: "must be at most 65536 bytes").
- `model`: a string of 1–256 characters (`config.model`).
- `connection_id`: a string that parses as a UUID (`config.connection_id`: "must be a UUID").
- `params`: an object whose keys are only those below (`config.params.<key>`: "is not a known parameter") with these types and ranges (inclusive), each error on `config.params.<key>` with message `must be <rule>`:

  | key | rule |
  |---|---|
  | `temperature` | number 0 to 2 |
  | `top_p` | number 0 to 1 |
  | `top_k` | integer 1 to 1000 |
  | `max_tokens` | integer 1 to 1000000 |
  | `presence_penalty`, `frequency_penalty` | number -2 to 2 |
  | `seed` | integer (any `i64`) |
  | `stop` | a string, or an array of 1 to 4 strings |
  | `reasoning_effort` | one of `minimal`, `low`, `medium`, `high` |

- `tools`: an array of at most 128 objects (`config.tools`); each `config.tools[i]` has keys among `name`, `description`, `parameters` only; `name` matches `^[A-Za-z0-9_-]{1,64}$` and is unique in the array ("must repeat no tool name"); `description` a string ≤ 4 096 characters; `parameters` a JSON object (required).
- `tool_choice`: a string: `auto`, `none`, `required`, or the `name` of one of `tools` (`config.tool_choice`: "must be auto, none, required or the name of a tool").
- `response_format`: an object with keys among `name`, `schema`, `strict`; `name` matches `^[A-Za-z0-9_-]{1,64}$`; `schema` a JSON object (required); `strict` a boolean.
- Any other top-level key is kept untouched.
- Return `serde_json::to_string(config)` (the input, unchanged: key order is preserved by the workspace's `preserve_order`).

Write the regex-like checks by hand with `chars().all(…)` (no `regex` crate in this module).

### 3. `api/prompts.rs`

Module doc: `//! Prompt registry request and response types (spec 09 § Prompts, D-V-2 to D-V-13).`

Derives: responses `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]`; requests add `#[serde(deny_unknown_fields)]`; query params `#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)] #[serde(deny_unknown_fields)]`. Every field has a doc comment. Optional response fields carry `#[serde(default, skip_serializing_if = "Option::is_none")]`. `template` fields are `serde_json::Value` with `#[schema(schema_with = any_json)]` (a string or an array). `config` fields use `#[schema(schema_with = open_object)]`, the helper of D-J-27 that `api/datasets.rs` uses for `metadata` (find its exact name with `grep -n "fn open_object" crates/server/src/api/*.rs`; if it has another name, use that one).

```rust
/// `text` or `chat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind { Text, Chat }

/// A label and the version it points at.
pub struct PromptLabel { pub label: String, pub version: i64,
    #[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64 }

/// A prompt (D-V-2).
pub struct Prompt {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")] pub kind: PromptKind,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub description: Option<String>,
    pub tags: Vec<String>,
    /// The highest version; `latest` always resolves to it (D-V-8).
    pub latest_version: i64,
    /// Stored labels, ordered by label. `latest` is not listed.
    pub labels: Vec<PromptLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub created_by: Option<String>,
    /* created_at, updated_at: times */
}

/// One version, as the fetch and the version routes return it (D-V-12).
pub struct PromptVersion {
    pub name: String,
    #[serde(rename = "type")] pub kind: PromptKind,
    pub version: i64,
    #[schema(schema_with = any_json)] pub template: Value,
    #[schema(schema_with = open_object)] pub config: Value,
    /// Labels pointing at this version, ordered; includes `latest` when this is the latest version.
    pub labels: Vec<String>,
    /// Variable names, in order of first appearance.
    pub variables: Vec<String>,
    /// Placeholder names (chat), in order of first appearance.
    pub placeholders: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub commit_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub created_by: Option<String>,
    /* created_at */
}

pub struct CreatePromptRequest {
    pub name: String,
    #[serde(rename = "type")] pub kind: PromptKind,
    #[schema(schema_with = any_json)] pub template: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")] #[schema(schema_with = open_object)] pub config: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub commit_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub tags: Vec<String>,
}

pub struct CreateVersionRequest {
    #[schema(schema_with = any_json)] pub template: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")] #[schema(schema_with = open_object)] pub config: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub commit_message: Option<String>,
}

/// Derives only `Deserialize` (+ `Debug, Clone, PartialEq, ToSchema`), as `UpdateDatasetRequest` does.
pub struct UpdatePromptRequest {
    /// `null` clears.
    #[serde(default, deserialize_with = "present")] #[schema(value_type = Option<String>)] pub description: Option<Option<String>>,
    #[serde(default)] pub tags: Option<Vec<String>>,
}

pub struct SetLabelRequest { pub version: i64 }

pub struct PromptLabelEvent {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub from_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub to_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub actor: Option<String>,
    /* at: time */
}

/// Both the request and the response of the settings routes; `#[serde(deny_unknown_fields)]`.
pub struct PromptSettings {
    /// Labels only project admins and `write` keys may move (D-V-10).
    pub protected_labels: Vec<String>,
}

pub struct PromptListParams {
    #[serde(default)] pub q: Option<String>,
    #[serde(default)] pub label: Option<String>,
    #[serde(default)] pub tag: Option<String>,
    #[serde(default)] #[param(minimum = 1, maximum = 1000, default = 50)] pub limit: Limit,
    #[serde(default)] pub cursor: Option<String>,
}

pub struct ResolveParams {
    /// A label; default `production`. `latest` resolves the highest version.
    #[serde(default)] pub label: Option<String>,
    /// A version number. Not together with `label`.
    #[serde(default)] pub version: Option<i64>,
}
```

(`PageParams` from `api::common` serves the versions and label-events lists.) Copy the private `present` deserializer from `api/datasets.rs` into this module (do not change `datasets.rs`). Write each struct with full derives and doc comments; the `/* … */` notes above stand for the usual time fields.

Two conversion functions in `api/prompts.rs` would need server-private types; put them in `routes/prompts.rs` instead (V1.4).

### Tests

In `prompts/mod.rs`:
1. `names` — accepted: `a`, `support-reply`, `v1.2_x`, `A`, a 200-char name; refused: ``, ` a`, `-a`, `.a`, `a/b`, `a b`, `é`, a 201-char name.
2. `labels` — accepted: `production`, `v1`, `a.b-c_d`, a 64-char label; refused: `Production`, `latest`, `123`, `-x`, ``, a 65-char label; messages as specified.
3. `check_labels_counts_and_duplicates` — 51 labels with max 50 → the count error; `["a","a"]` → duplicate error on `labels[1]`.
4. `tags_trim_dedupe_and_cap`.
5. `text_body` — `"Hi {{ name }}"` parses with variables `["name"]`; `"{% x"` gives a `template` error with position 1:1 and message `Reason::Block.message()`; `42` gives "must be a string for a text prompt".
6. `chat_body` — the `support-reply` v1 template of § The example prompts parses with variables `["product", "question"]`, placeholders `["history"]`; `to_json` round-trips to the same JSON; errors: `[]`, an entry `{"role":"tool","content":"x"}`, `{"role":"user"}`, `{"role":"user","content":"x","name":"y"}`, `{"type":"other","name":"h"}`, `{"type":"placeholder","name":"1h"}`, a content `"{{ a.b }}"` (error on `template[0].content` with position), a name used as both variable and placeholder.
7. `body_limits` — a 256 KiB + 1 text template; 201 chat entries; 101 distinct variables.
8. `config_rules` — the example configs pass and return their text; one test per error row: `temperature` 3, `top_p` -0.1, `top_k` 0, `max_tokens` 1.5, `seed` "x", `stop` `["a","b","c","d","e"]`, `reasoning_effort` "max", unknown param `foo`, a tool without `parameters`, a duplicate tool name, `tool_choice` `"get_weather"` with no such tool, `response_format` without `schema`, `connection_id` "x", `model` "", a non-object config; an unknown top-level key `team: "x"` is kept.
9. `debug_hides_templates` — `format!("{:?}", PromptBody::Text("canary-debug".into()))` has no `canary-debug`.

In `api/prompts.rs`: `requests_refuse_unknown_fields` (`CreatePromptRequest` with an extra `foo` fails), `update_tri_state` (`{}` → `None`, `{"description": null}` → `Some(None)`, `{"description":"x"}` → `Some(Some("x"))`), `kind_spelling` (`"text"`/`"chat"`).

### Done when

- `cargo test -p cetra-server --lib prompts`, `cargo test -p cetra-server --lib roles`, `pnpm --dir web exec vitest run src/lib/roles.test.ts --maxWorkers=8` pass; `just check` is green.

---

## V1.4 — Prompt, version and fetch routes

**Commit subject:** `server: add the prompt, version and fetch routes`

### Read first

- `crates/server/src/routes/datasets.rs`: `routes()` (the `RouteDef` table), `list_datasets`, `create_dataset`, `get_dataset`, `update_dataset`, `delete_dataset`, `created_by`, `find`, `bad_request`, the audit lines, the test module's first tests and `audit_log_names_the_key`.
- `crates/server/src/routes/orgs.rs` `page_named`.
- `crates/server/src/http/access.rs` `routes()` (the list of `routes.extend`), `ProjectAccess`; `crates/server/src/auth/principal.rs` `Principal`.
- `crates/server/src/api/openapi.rs`: `dataset_paths`, `collect_schemas` (dataset entries), the tag list, `NULL_IS_A_VALUE`, helpers `operation`, `path_string`, `project_param`, `with_query`, `responses`, `json_named`, `json_of`, `json_body`.
- `crates/server/tests/openapi.rs`: `OPERATIONS`, `TAGS`, `QUERY_OR_BODY`, the error-code `else if` chain in `document_lint`, the `pages` list.
- `crates/server/src/api_examples.rs` (`generate`, `fetch`, `Output::add`, the hand-listed `example_created` entries), `crates/server/tests/api_examples.rs` (`manifest_covers_the_read_operations`, the response round-trip table, the `requests` table), `crates/server/src/test_support/mod.rs` `seed_example_faq_dataset`, `fixtures/api/README.md` § Fixed identifiers.

### Edit

| File | Change |
|---|---|
| `crates/server/src/routes/prompts.rs` | new |
| `crates/server/src/routes/mod.rs` | `pub(crate) mod prompts;` (alphabetical) |
| `crates/server/src/http/access.rs` | `routes.extend(crate::routes::prompts::routes());` after the datasets/experiments lines |
| `crates/server/src/prompts/mod.rs` | remove the `cfg_attr(…allow(dead_code))` line |
| `crates/server/src/api/openapi.rs` | tag `prompts`, `fn prompt_paths`, schemas, chained in `paths()` after `dataset_paths` |
| `crates/server/tests/openapi.rs` | operations, tag, `QUERY_OR_BODY`, error branches, pages |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | regenerated by `just openapi` |
| `crates/server/src/test_support/mod.rs` | `seed_example_prompts` |
| `crates/server/src/api_examples.rs` | prompt examples |
| `crates/server/tests/api_examples.rs` | round-trips and request examples |
| `fixtures/api/…` | regenerated (`just api-examples`) and two request files |
| `fixtures/api/README.md` | the `…1001`/`…1002` ids in § Fixed identifiers |

### 1. Routes in this phase

Base path `/api/v1/projects/{project}`. Path struct `#[derive(Debug, Deserialize)] pub(crate) struct PromptPath { pub(crate) prompt: String }`; `PromptVersionPath { prompt: String, version: i64 }`.

| Method, path | Access | Handler | Answer |
|---|---|---|---|
| `GET /prompts` | `View` | `list_prompts` | `200 Page<Prompt>` |
| `POST /prompts` | `ManagePrompts` | `create_prompt` | `201 PromptVersion` (version 1) |
| `GET /prompts/{prompt}` | `View` | `get_prompt` | `200 Prompt` |
| `PATCH /prompts/{prompt}` | `ManagePrompts` | `update_prompt` | `200 Prompt` |
| `DELETE /prompts/{prompt}` | `ManagePrompts` | `delete_prompt` | `204` |
| `GET /prompts/{prompt}/resolve` | `View` | `resolve_prompt` | `200 PromptVersion` with `ETag`, or `304` |
| `GET /prompts/{prompt}/versions` | `View` | `list_versions` | `200 Page<PromptVersion>` |
| `POST /prompts/{prompt}/versions` | `ManagePrompts` | `create_version` | `201 PromptVersion` |
| `GET /prompts/{prompt}/versions/{version}` | `View` | `get_version` | `200 PromptVersion` |

Every row: `session_only: false`, `documented: true`. Body limit on the two POSTs and the PATCH: `.layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT))` with `const PROMPT_BODY_LIMIT: usize = 512 * 1024;`.

### 2. Imports and shared helpers in `routes/prompts.rs`

Imports as `routes/datasets.rs` does: `use crate::meta::prompts as store;` (the handler names collide with the store's), the extractors `crate::http::error::{self, ApiError}` with `error::Json`, `error::Path`, `error::Query` (never axum's own, so rejections are problem+json), `crate::http::access::{Access, ProjectAccess, RouteDef}`, `crate::http::AppState`, `crate::auth::principal::Principal`, `crate::auth::roles::{ProjectPerm, Role}`, `crate::api::common::{FieldError, Page, PageParams, ProblemCode}` (whatever subset you use; `ProblemCode` lives where `ApiError::Forbidden` takes it from), `super::datasets::created_by`, `super::orgs::page_named`.


```rust
/// The prompt named by the path, or 404.
async fn find(state: &AppState, project: ProjectId, name: &str) -> Result<PromptRow, ApiError>;

/// D-V-10: a project admin, or an API key (a key that reaches a `ManagePrompts` route has `write`).
fn may_touch_protected(principal: &Principal, access: &ProjectAccess) -> bool {
    matches!(principal, Principal::ApiKey { .. }) || access.role == Some(Role::Admin)
}

/// `Prompt` JSON of a row.
fn prompt_json(row: &PromptRow) -> Prompt;

/// `PromptVersion` JSON. `labels` = the version's stored labels plus `latest` when `version.version == prompt.latest_version`, sorted.
/// The stored template and config texts are parsed back (`Corrupt` → 500) and the body re-parsed
/// with `prompts::parse_body` for `variables`/`placeholders` (a stored body that no longer parses is
/// `MetaError::Corrupt { table: "prompt_versions", column: "template" }`).
fn version_json(prompt: &PromptRow, version: &VersionRow) -> Result<PromptVersion, ApiError>;

/// Validates a template + config + labels + commit message for `prompt_type`, returning the
/// `NewVersion` (errors collected from every field).
fn new_version(prompt_type: PromptType, template: &Value, config: Option<&Value>, labels: &[String],
    commit_message: Option<&str>, actor: Option<String>) -> Result<NewVersion, ApiError>;
```

`new_version`: `parse_body` → body; `config` default `{}` → `check_config`; `check_labels("labels", labels, MAX_LABELS_PER_REQUEST)`; `check_commit_message`. Any error → `ApiError::BadRequest { detail: None, errors }` with all errors collected. Template text = `serde_json::to_string(&body.to_json())`.

**Protected check** (D-V-10), used by create, create version, delete (and V1.5's label routes): inside the write closure, read `protected_labels_tx`; if any label the request sets (or, for delete, any label the prompt carries) is in that list and `!may_touch_protected(..)`, return `Err(ProtectedLabel)`. Since the `write` closure returns `MetaError`, compute `may` before the write and pass it in; on refusal return `Ok(Outcome::Forbidden)` from the closure (a small private enum per handler) and map it to `ApiError::Forbidden { code: ProblemCode::Forbidden }` after the write. Nothing is written in that case.

### 3. Handlers, in order of operations

- **`list_prompts`**: validate `label` with `check_label("label", …)` when present (400), map `latest` to `None`; `tag` and `q` are passed as given (`q` trimmed, empty → None). `store::list_prompts`, then `page_named` with `(name.to_lowercase(), id.to_string(), prompt_json(&row))`, `params.cursor`, `params.limit.get()`.
- **`create_prompt`**: `check_name(&body.name)`, `check_description` (when given), `check_tags`, and `new_version(kind, …, created_by(&principal))`; collect all field errors into one 400. Then one `state.meta.write`: protected check over `body.labels`; `store::create_prompt(conn, project, NewPrompt { id: Uuid::now_v7(), … }, version, now)`. Audit `"prompt created"` with `prompt_id`, `version = 1`. Answer `201` with `version_json`.
- **`get_prompt`**: `find` → `prompt_json`.
- **`update_prompt`**: validate `description` (inner `Some` through `check_description`; an empty string clears) and `tags`; `store::update_prompt` → `None` = 404. Audit `"prompt updated"`.
- **`delete_prompt`**: one write: `get_prompt_tx` (404 when absent), protected check over the prompt's labels, `delete_prompt`. Audit `"prompt deleted"`. `204`.
- **`resolve_prompt`** (D-V-12):
  1. `label` and `version` both present → 400 field `version`: "give label or version, not both". `label` present → `check_label("label", …)` unless it is `latest`.
  2. `find` the prompt. Version present → `get_version`; label `latest` → `get_version(prompt.latest_version)`; else `resolve_label(label or DEFAULT_LABEL)`. None → `ApiError::NotFound`.
  3. Body bytes = `serde_json::to_vec(&version_json(..)?)`. `etag = format!("\"{}\"", hex::encode(&Sha256::digest(&bytes)[..16]))`.
  4. If the request header `If-None-Match` (take `headers: HeaderMap` as an extractor), split on `,`, each entry trimmed and stripped of a leading `W/`, contains `etag` or `*` → answer `304` with headers `ETag` and `Cache-Control: no-cache` and an empty body.
  5. Else `200`, `Content-Type: application/json`, `ETag`, `Cache-Control: no-cache`, the bytes. Return type `Result<Response, ApiError>` built with `(StatusCode, [(HeaderName, String); N], Vec<u8>).into_response()`.
- **`list_versions`**: `find`; `cursor` = the decimal of the last version of the previous page (`None` first); a cursor that does not parse as a positive `i64` → 400 field `cursor`: "invalid cursor". `store::list_versions(pool, id, before, limit)`; if `limit + 1` rows came back, drop the last and set `next_cursor` to the last kept version's decimal. Map each with `version_json`.
- **`create_version`**: `find` (outside the write, for the type) then `new_version(prompt.prompt_type, …)`; one write: `get_prompt_tx` again (404 if deleted meanwhile), protected check over `body.labels`, `store::add_version`. Audit `"prompt version created"` with `version`. `201`. A template whose shape does not fit the prompt's type is reported by `parse_body` (field `template`).
- **`get_version`**: `find`; `get_version` → 404 when absent; `version_json`.

### 4. OpenAPI

Tag: `tag("prompts", "The prompt registry: versioned prompts with labels (D-V-2).")`, added to the tag list next to `datasets`. Schemas in `collect_schemas`: `Prompt`, `PromptKind`, `PromptLabel`, `PromptVersion`, `CreatePromptRequest`, `CreateVersionRequest`, `UpdatePromptRequest`, `insert_page::<Prompt>("PromptPage")`, `insert_page::<PromptVersion>("PromptVersionPage")`. `NULL_IS_A_VALUE` gets `("UpdatePromptRequest", "description")`.

Constants: `PROMPT_READ = "Any project member, or an API key with `read`."`, `PROMPT_WRITE = "Project members and above, a read-write personal access token, or an API key with `write` (D-V-13). Protected labels need a project admin or a `write` key (D-V-10)."`. `let prompt = path_string("prompt", "Prompt name.");` and an integer `version` path parameter built like the dataset version parameter.

| Operation id | Method, path | Responses (success; errors) | `QUERY_OR_BODY` |
|---|---|---|---|
| `list_prompts` | GET `/prompts` | 200 `PromptPage`; 400 401 403 404 429 | yes |
| `post_prompt` | POST `/prompts` | 201 `PromptVersion`; 400 401 403 404 409 413 429 | — |
| `get_prompt` | GET `/prompts/{prompt}` | 200 `Prompt`; 401 403 404 429 | — |
| `patch_prompt` | PATCH `/prompts/{prompt}` | 200 `Prompt`; 400 401 403 404 413 429 | — |
| `delete_prompt` | DELETE `/prompts/{prompt}` | 204; 401 403 404 429 | — |
| `get_prompt_resolve` | GET `/prompts/{prompt}/resolve` | 200 `PromptVersion` (describe the `ETag` header in the description), 304 (description "Not modified: the If-None-Match tag matches."); 400 401 403 404 429 | yes |
| `list_prompt_versions` | GET `/prompts/{prompt}/versions` | 200 `PromptVersionPage`; 400 401 403 404 429 | yes |
| `post_prompt_version` | POST `/prompts/{prompt}/versions` | 201 `PromptVersion`; 400 401 403 404 409 413 429 | — |
| `get_prompt_version` | GET `/prompts/{prompt}/versions/{version}` | 200 `PromptVersion`; 401 403 404 429 | — |

In `tests/openapi.rs`: add each row to `OPERATIONS`; `"prompts"` to `TAGS`; the `yes` rows to `QUERY_OR_BODY`; `post_prompt` and `post_prompt_version` to the branch that already expects `["400","401","403","404","409","413","429"]` (the `post_dataset` branch); `patch_prompt` to the branch expecting `400 401 403 404 413 429` if one exists, else add one `else if` with exactly those; no change for `delete_prompt` (it declares the default set `401 403 404 429`); `("PromptPage","Prompt")` and `("PromptVersionPage","PromptVersion")` to the `pages` tuple list (around line 1237). `assert_nullability` (around lines 1488–1560) has its own `NULL_IS_A_VALUE` list and its own list of page names: add `("UpdatePromptRequest", &["description"])` to the first and `"PromptPage"`, `"PromptVersionPage"` to the second.

Then `just openapi`.

### 5. Example prompts and fixtures

- `TestApp::seed_example_prompts(&self)` next to `seed_example_faq_dataset`: writes § The example prompts through `store::create_prompt` / `store::add_version` inside `state.meta.write`, on `EXAMPLE_PROJECT`, with the fixed ids and times given there, actor `None`. Templates and configs go through `prompts::parse_body` / `check_config` so they are stored exactly as the API would.
- `api_examples.rs`: `const EXAMPLE_PROMPT: &str = "support-reply";` Call `app.seed_example_prompts()` in `generate()` after the dataset seed. Add (paths relative to the project base):
  - `list_prompts` → `responses/prompts.json` (`/prompts`);
  - `get_prompt` → `responses/prompts/support-reply.json`;
  - `get_prompt_resolve` → `responses/prompts/support-reply/resolve.json` (`/prompts/support-reply/resolve`, the production version 1), and a second entry with query `label=staging` → `responses/prompts/support-reply/resolve-staging.json`;
  - `list_prompt_versions` → `responses/prompts/support-reply/versions.json`;
  - `get_prompt_version` → `responses/prompts/support-reply/versions/2.json`.
  - one more `out.add("get_prompt_version", …)` with path params `prompt` = `support-reply`, `version` = `1` → `responses/prompts/support-reply/versions/1.json`;
  - hand-listed: `example_created("post_prompt", BTreeMap::new(), "responses/prompts/support-reply/versions/1.json")` and `example_created("post_prompt_version", params(&[("prompt", EXAMPLE_PROMPT)]), "responses/prompts/support-reply/versions/2.json")`. If the helpers' signatures differ, follow the shape of the datasets entries.
- `fixtures/api/requests/prompt-create.json`: `{"name":"support-reply","type":"chat","template":[…v1 template…],"config":{"model":"gpt-4o","params":{"temperature":0.3,"max_tokens":512}},"labels":["production"],"commit_message":"First version"}`; `fixtures/api/requests/prompt-version-create.json`: the v2 template, its config, `"labels":["staging"]`, `"commit_message":"Friendlier tone"`. Both files are canonical JSON as the round-trip check writes it: pretty-printed with 2 spaces, a trailing newline, keys in struct field order (`name`, `type`, `template`, `config`, `labels`, `commit_message`, then `description`, `tags` when present). Register both in `tests/api_examples.rs`'s `requests` table (`post_prompt`, `post_prompt_version`) and the request round-trips (`CreatePromptRequest`, `CreateVersionRequest`). Response round-trips per operation id: `list_prompts` → `Page<Prompt>`; `get_prompt` → `Prompt`; `get_prompt_resolve`, `get_prompt_version`, `post_prompt`, `post_prompt_version` → `PromptVersion`; `list_prompt_versions` → `Page<PromptVersion>`. `generated_files_roundtrip` panics on an operation id it does not know.
- Run `just api-examples`. `fixtures/api/README.md` § Fixed identifiers gets the two prompt ids.

### Tests (`routes/prompts.rs`, `#[cfg(test)] mod tests`)

Use `TestApp`, Ada's cookie (org owner → project admin), a member, a viewer, `write_key`, `read_key`, `ingest_key`. Let `base = format!("/api/v1/projects/{}/prompts", seeded.project.id)`.

1. `create_get_and_list` — POST the `prompt-create.json` body → 201, `version == 1`, `labels == ["latest", "production"]` (sorted), `variables == ["product","question"]`, `placeholders == ["history"]`; GET `/{name}` → `latest_version == 1`, labels `[production → 1]`; GET list → one prompt; `?q=SUPPORT` finds it; `?label=staging` finds none; `?label=latest` finds it.
2. `versions_and_resolve` — POST a version with `labels: ["staging"]` → 201 version 2; `resolve` (no query) → version 1; `?label=staging` → 2; `?label=latest` → 2; `?version=1` → 1; `?version=9` → 404; `?label=nope` → 404; `?label=a&version=1` → 400 field `version`; `?label=BAD` → 400 field `label`; `versions` list → 2, 1; `?limit=1` → one item and a `next_cursor`, and the next page → version 1 with no cursor; `versions/2` → 2.
3. `resolve_etag` — first `resolve` → 200 with an `etag` header and `cache-control: no-cache`; again with `If-None-Match: <etag>` → 304 and an empty body; with `If-None-Match: "other", <etag>` → 304; with `"other"` → 200; after moving `production` to version 2 (POST a version with `labels: ["production"]` as Ada) the old tag gives 200 and a new tag.
4. `validation_errors` — one create with a bad name (`a b`), a `{% %}` text template, a bad config `temperature: 5` and label `latest`: one 400 whose `errors` list `name`, `template` (with a position), `config.params.temperature` and `labels[0]`; a second create with `type: "text"` and an array template → 400 field `template`; a duplicate name → 409; a version whose template does not fit the type → 400 field `template`.
5. `permissions` — viewer: GET list 200, POST 403; member: POST without labels 201; `read_key`: GET resolve 200, POST 403; `ingest_key`: GET resolve 403; `write_key`: POST 201; a read-only PAT POST 403; a read-write PAT of the member POST 201.
6. `protected_labels` — member POST create with `labels: ["production"]` → 403 and no prompt created (GET → 404); member create without labels, then POST version with `labels: ["production"]` → 403 (latest_version stays 1); member version with `labels: ["staging"]` → 201; `write_key` version with `["production"]` → 201; Ada (admin) → 201; member DELETE of a prompt carrying `production` → 403; member DELETE of a prompt with only `staging` → 204.
7. `patch_and_delete` — PATCH `description` then `{"description": null}` clears it; PATCH tags `[" a ", "a", "b"]` → `["a","b"]`; DELETE → 204, GET → 404, DELETE again → 404.
8. `other_project_is_404` — a prompt created in another project (`app.add_project`, then Ada's cookie) is 404 through the first project's base path; `app.write_key(&seeded)` (project A) gets 404 on project B's prompt path.
9. `audit_log_has_no_payload` — inside `capture_logs`, create a prompt named `canary-name-x` whose template is `canary-template-y` with tag `canary-tag-z` and commit message `canary-commit-w`, using `write_key`; the logs contain `prompt created` and `actor=key:`, and none of the four canaries.

### Done when

- `cargo test -p cetra-server --lib routes::prompts`, `cargo test -p cetra-server --test openapi`, `cargo test -p cetra-server --test api_examples` and `cargo test -p cetra-server --lib api_examples` pass; `just openapi` and `just api-examples` leave no diff; `just check` is green.

---

## V1.5 — Labels, label history, protected-label settings; specs and changelog

**Commit subject:** `server: add prompt label routes, label history and protected-label settings`

### Read first

- V1.4's `routes/prompts.rs` (helpers, protected check, the OpenAPI block).
- `docs/spec/04-metadata-db.md` § Datasets & experiments, `docs/spec/09-api.md` § Datasets & experiments, `docs/spec/08-auth.md` (the paragraph listing who manages datasets), `CHANGELOG.md` § `[Unreleased]` › `### Added`.

### Edit

| File | Change |
|---|---|
| `crates/server/src/routes/prompts.rs` | 5 routes, tests |
| `crates/server/src/api/prompts.rs` | nothing new unless a type is missing (all are defined in V1.3) |
| `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs` | 5 operations, schemas `SetLabelRequest`, `PromptLabelEvent`, `PromptSettings`, page `PromptLabelEventPage` |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | `just openapi` |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `fixtures/api/…` | examples |
| `docs/spec/04-metadata-db.md` | new § Prompts (the migration's SQL and the rules) |
| `docs/spec/09-api.md` | new § Prompts (the route table) |
| `docs/spec/08-auth.md` | one paragraph on prompt permissions |
| `CHANGELOG.md` | one `### Added` line |
| `docs/roadmap.md` | V row: status |

### 1. Routes

| Method, path | Access | Handler | Answer |
|---|---|---|---|
| `PUT /prompts/{prompt}/labels/{label}` | `ManagePrompts` | `set_label` | `200 Prompt` |
| `DELETE /prompts/{prompt}/labels/{label}` | `ManagePrompts` | `remove_label` | `204` |
| `GET /prompts/{prompt}/label-events` | `View` | `list_label_events` | `200 Page<PromptLabelEvent>` |
| `GET /prompt-settings` | `View` | `get_settings` | `200 PromptSettings` |
| `PUT /prompt-settings` | `ManageSettings` | `put_settings` | `200 PromptSettings` |

Path struct `PromptLabelPath { prompt: String, label: String }`.

- **`set_label`**: `check_label("label", &label)` (path value; `latest` → its message) → 400. `body.version < 1` → 400 field `version`: "must be a positive version". One write: `get_prompt_tx` (404), protected check for `label`, `store::set_label(conn, id, label, version, actor, now)`: `None` → 404. Read back with `get_prompt_tx`. Audit `"prompt label set"` with `prompt_id`, `version` (not the label). `200 prompt_json`.
- **`remove_label`**: `check_label`; one write: `get_prompt_tx` (404), protected check for `label`, `store::remove_label` false → 404. Audit `"prompt label removed"`. `204`.
- **`list_label_events`**: `find`; cursor = the decimal `id` of the last event of the previous page (parse error → 400 `cursor`); paging as `list_versions`.
- **`get_settings`**: `store::protected_labels`.
- **`put_settings`**: body `PromptSettings`; `check_labels("protected_labels", &labels, MAX_PROTECTED)`; `set_protected_labels`; audit `"prompt settings updated"` with the label count. `200` with the stored list.

In the protected check of every route (V1.4's and these), a project admin is `access.role == Some(Role::Admin)`; `ProjectPerm::ManageSettings` already restricts `put_settings` to admins (a key cannot call it).

### 2. OpenAPI

| Operation id | Method, path | Responses | `QUERY_OR_BODY` |
|---|---|---|---|
| `put_prompt_label` | PUT `/prompts/{prompt}/labels/{label}` | 200 `Prompt`; 400 401 403 404 409 413 429 | — |
| `delete_prompt_label` | DELETE `/prompts/{prompt}/labels/{label}` | 204; 400 401 403 404 429 | — |
| `list_prompt_label_events` | GET `/prompts/{prompt}/label-events` | 200 `PromptLabelEventPage`; 400 401 403 404 429 | yes |
| `get_prompt_settings` | GET `/prompt-settings` | 200 `PromptSettings`; 401 403 404 429 | — |
| `put_prompt_settings` | PUT `/prompt-settings` | 200 `PromptSettings`; 400 401 403 404 413 429 | — |

Place each write in the `document_lint` branch with exactly its error set; add a new `else if` only when no branch has that exact set. `delete_prompt_label` (`400 401 403 404 429`) goes in the existing branch that holds `get_evaluator || delete_score_definition`; `get_prompt_settings` needs no change (default set). `label` is a string path parameter ("Label name."). `put_prompt_settings`' description: "Project admins (D-V-10)."

In `tests/openapi.rs` also add `("PromptLabelEventPage","PromptLabelEvent")` to the `pages` tuple list and `"PromptLabelEventPage"` to `assert_nullability`'s page-name list.

Then `just openapi`.

### 3. Examples

- Responses: `list_prompt_label_events` → `responses/prompts/support-reply/label-events.json`; `get_prompt_settings` → `responses/prompt-settings.json` (the default `["production"]`).
- Static or hand-listed: `put_prompt_label` → reuse `responses/prompts/support-reply.json` with status 200 (`example("put_prompt_label", params(&[("prompt", EXAMPLE_PROMPT), ("label", "staging")]), BTreeMap::new(), "responses/prompts/support-reply.json", "application/json")` or the helper that fits); `put_prompt_settings` → `responses/prompt-settings.json`.
- Requests (canonical JSON, pretty-printed): `fixtures/api/requests/prompt-label-set.json` `{"version": 2}`, `fixtures/api/requests/prompt-settings.json` `{"protected_labels": ["production"]}`, registered in the `requests` table and round-tripped (`SetLabelRequest`, `PromptSettings`). Response round-trips: `put_prompt_label` → `Prompt`; `get_prompt_settings`, `put_prompt_settings` → `PromptSettings`; `list_prompt_label_events` → `Page<PromptLabelEvent>`.
- `just api-examples`.

### 4. Specs, changelog, roadmap

- `docs/spec/04-metadata-db.md`: a new `## Prompts (track V)` section after `## Datasets & experiments`: the migration's SQL verbatim in a `sql` block, then four bullets: versions are immutable and numbered from 1 (D-V-7); `latest` is computed, never stored (D-V-8); label moves are recorded in `prompt_label_events` (D-V-8); a project without a `prompt_settings` row protects `production` (D-V-10).
- `docs/spec/09-api.md`: a new `### Prompts` section after `### Datasets & experiments` with one table row per route of V1.4 and V1.5 (path, who, what), in the style of the datasets table; mention the `ETag`/`304` of `resolve` and that errors use the existing codes.
- `docs/spec/08-auth.md`: in the permission table (around lines 100–106) add a row `| Read prompts · create versions and move unprotected labels · move protected labels, edit the protected list | ✓ · · | ✓ ✓ · | ✓ ✓ ✓ | ✓ ✓ ✓ |` in the table's own column style (viewer, member, admin, owner; adapt the cell format to the table you find), and after the datasets/evaluators permission paragraph: "Prompts: every project member and a `read` key read them; members and above, read-write PATs and `write` keys create versions and move labels (`ProjectPerm::ManagePrompts`); protected labels need a project admin or a `write` key; project admins edit the protected list (D-V-10, D-V-13)."
- `CHANGELOG.md` `### Added`: `- Prompt registry API (D191, D-V-2 to D-V-13): versioned text and chat prompts with plain {{var}} templates, labels with protected labels, label history, and a fetch with ETag at GET /prompts/{name}/resolve.`
- `docs/roadmap.md` V row status: `**In progress.** M1 (registry API) implemented.`
- The user documentation page for prompts comes with M2 (SDK) and M4 (UI); M1's routes appear in the generated API reference only.

### Tests (`routes/prompts.rs`)

1. `labels_move_and_history` — on the seeded-style prompt (create it in the test with labels `production`), Ada PUT `staging` → version 2 (after adding version 2) → 200, labels include `staging → 2`; PUT `production` → 2; DELETE `staging` → 204; DELETE again → 404; PUT to version 9 → 404; PUT `latest` → 400; label-events → newest first: staging removed (to absent), production 1→2, staging new→2, production new→1; `?limit=2` pages.
2. `label_protection` — member PUT `production` → 403; member PUT `staging` → 200; member DELETE `production` → 403; `write_key` PUT `production` → 200.
3. `settings` — GET → `["production"]`; member PUT → 403; `write_key` PUT → 403; Ada PUT `["prod"]` → 200; now member PUT `production` → 200 and member PUT `prod` → 403; Ada PUT `["latest"]` → 400; 21 labels → 400.
4. `label_cap_conflict` — create a prompt with no label, then Ada PUTs labels `l00` … `l49` (50 labels) on version 1, all 200; a 51st, `l50`, → 409.

### Done when

- `cargo test -p cetra-server --lib routes::prompts`, `--test openapi`, `--test api_examples` pass; `just openapi` and `just api-examples` leave no diff; `just check` is green.
