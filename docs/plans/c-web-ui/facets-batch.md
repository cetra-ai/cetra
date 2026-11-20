Status: planned 2026-11-20

# Facets in one request: `POST /facets`

Opening the Spans page sends six `GET /facets` requests: `span_kind`, `status` and `name` (`limit=10`) for the open value sections, `duration_ms` for the histogram, and `duration_ms` and `cost_total` (`limit=1`) for the p95 of the Slow and Expensive preset chips. This plan replaces `GET /api/v1/projects/{project}/facets` with `POST /api/v1/projects/{project}/facets`: the body holds the window, the flags, the filter and the search once, plus a list of fields; the answer has one result per field, in request order, each with its own error. The storage layer builds the list's rows once and computes the fields concurrently. In the web UI each facet keeps its own TanStack Query entry per field; a small batcher gathers the fields asked within 10 ms and sends one POST per distinct shared part, then splits the answer back into each field's query. `GET /facets` is removed with no compatibility path (AGENTS.md).

**Prerequisites:** none. Branch `claude/facets-batch` from `origin/main` (713367d), in `.claude/worktrees/facets-batch`.

**Read first:**
- `AGENTS.md` (rules, privacy, tests, "No compatibility layers").
- `docs/spec/05-query.md` § Facets; `docs/spec/09-api.md` line with `GET /facets`; `docs/spec/11-ui.md` § Facet sidebar and § Presets; `docs/mvp-decisions.md` D-C-13.
- `crates/storage/src/reads/facets.rs` (all of it), `crates/storage/src/reads/mod.rs` and `crates/storage/src/lib.rs` (the `facets` re-exports).
- `crates/server/src/api/facets.rs`, `crates/server/src/routes/facets.rs`, `crates/server/src/routes/read.rs` (`Deadline`, `check_search`, `list_scope`, `open_query`), `crates/server/src/http/error.rs` (`ApiError::QueryAt`, `ApiError::log`, `ApiError::problem`, `query_problem`), `crates/server/src/routes/dashboards.rs` lines 40–50 and 130–145 (a POST read route with a body limit), `crates/server/src/api/openapi.rs` (`get_facets` around line 2553 and `query_widget` around line 4991), `crates/server/src/api_examples.rs` (the `FACETS` loop and `post_json`).
- `web/src/api/queries.ts` (`FacetAsk`, `facetQuery`, `useFacetQuery`, `latestFacetAnswer`), `web/src/api/client.ts`, `web/src/api/problem.ts`.
- The callers: `web/src/components/facets/{values-facet,range-facet,keys-facet,preset-chips}.tsx`, `web/src/components/filter-bar/{editor,value-input}.tsx`.
- The mocks: `web/src/mocks/handlers.ts` (`widgetQueryFixture`), `web/src/mocks/generated/handlers.ts` (`facets`, `facetAnswer`, `readQuery`, `problem`).

## What it is, and what it is not

When it is done:
- `POST /api/v1/projects/{project}/facets` answers a batch of 1 to 20 fields over one list's rows; `GET` on that path is 405 (no route).
- The Traces, Spans, Sessions and Users pages send one `/facets` request when they open with no filter, and one request with only the changed field when a section opens, its value search changes, or "Show more" raises its limit.
- The filter bar's value autocomplete and the chip editor's value suggestions send a batch of one field.
- Specs 05, 09 and 11, D-C-13 and a new decision D-C-23, the CHANGELOG, the OpenAPI document, the generated TS schema, the API examples and the MSW mocks describe and serve the batch.

Out of scope:
- The list row queries and their caching (another session owns them).
- A server-side cache of facet answers (D-C-13 keeps none).
- One scan for all fields (see D-C-23: rejected for now).
- A per-field `filter`. A section asks its counts with the filter minus its own chips (spec 11), so when the filter is not empty, sections whose own chips differ ask with different filters, and the batcher sends one request per distinct filter. With an empty filter (opening a page with no filter) all fields share one request.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-13 | Facets: one field per request (changed by D-C-23), counted over the list's rows, no server cache, UI keeps answers 30 s. |
| D129 | The filter text is the truth; values autocomplete from facets. |
| D-H-4 | A read made with POST uses `ProjectPerm::Query`: a read-only PAT and a `read` key may call it. |
| D-C-23 (new) | `POST /facets` batches 1–20 fields sharing view, window, flags, filter and search; one result per field in request order, each with its own `facet` or `error` (a problem document). Shared problems (bad filter, search on sessions, bad window, empty or too many fields) fail the whole request. Storage builds the rows once and computes the fields concurrently under one deadline. The web client keeps one TanStack Query entry per field and a 10 ms batcher sends one POST per distinct shared part. Rejected: caching the window's rows (`DataFrame::cache`) for a single scan (memory grows with the window); one combined aggregate plan over heterogeneous fields (list unnest, keys, histograms each need their own grouping; complex for a small gain); keeping `GET` for single fields (two paths for one thing, AGENTS.md). |

User decisions (from the session that started this one, relayed from the user): batch with a POST body holding the shared window and filters once plus `fields: [{ field, limit?, q? }]`; one result per field in request order, each with its own error; `GET /facets` removed with no compatibility layer; every caller moves to the batch including the filter bar (batch of one); client cache stays per field, with a batcher for fields asked in the same frame; keep `staleTime`, `keepPreviousData` and the latest-cached-answer placeholder.

Plan-level choices (details, not design changes): at most 20 fields per batch (`MAX_FACET_FIELDS`); body limit 64 KiB like the widget query; batch delay 10 ms; the preset chips stop passing `limit: 1` (a histogram ignores `limit`), so their query key equals the range facet's when the filter is empty and TanStack Query shares one entry.

## Versions

No new dependency. `futures` is already a dependency of `cetra-storage` (`futures.workspace = true`, root pin `0.3.34`); P1 uses `futures::future::join_all`.

## API contract

Request (`FacetsRequest`):

```json
{
  "view": "spans",
  "from": "2026-01-01T00:00:00Z",
  "to": "2026-01-02T00:00:00Z",
  "filter": "status = 'error'",
  "search": "refund",
  "include_eval": false,
  "include_experiments": false,
  "fields": [
    { "field": "span_kind", "limit": 10 },
    { "field": "name", "limit": 10, "q": "chat" },
    { "field": "duration_ms" }
  ]
}
```

Only `view` and `fields` are required. Unknown members are `400 bad_request` (`deny_unknown_fields`, on both structs).

Answer (`FacetsResponse`), `200`:

```json
{
  "view": "spans",
  "results": [
    { "field": "span_kind", "facet": { "kind": "values", "total": 12, "missing": 0, "values": [{ "value": "llm", "count": 6 }], "more": false } },
    { "field": "name", "error": { "type": "about:blank", "title": "Bad Request", "status": 400, "code": "bad_request", "detail": "q must be 1 to 200 characters" } },
    { "field": "duration_ms", "facet": { "kind": "histogram", "total": 12, "missing": 0, "histogram": { "min": 1.0, "max": 9.0, "p50": 3.0, "p95": 8.0, "p99": 9.0, "buckets": [] } } }
  ]
}
```

- `results` has exactly one entry per `fields` entry, in the same order; duplicates are answered twice.
- Each result has `field` (as requested) and exactly one of `facet` or `error`.
- `error` is a problem document (`Problem`): per-field errors are `bad_request` (field cannot be faceted, bad `limit`, bad `q`), `query_timeout` (that field did not finish before the deadline), `query_failed`, `query_too_large` or `internal`. A per-field `invalid_filter` cannot happen (the filter is parsed once, before any field). The field errors use the field path `fields[<index>]` where the problem carries `errors` (only for `invalid_filter`, so in practice no `errors`).
- Whole-request errors (problem response, no `results`): `400 bad_request` for a malformed body, a bad `from`/`to`, a `search` over 1,024 bytes, `search` on sessions, or `fields` empty or longer than 20; `400 invalid_filter` for a bad filter (field `filter`); `422 query_timeout` when building the rows times out; `401`, `403`, `404`, `413` (body over 64 KiB), `415`, `429`, `503` as other POST reads.
- The access is `ProjectPerm::Query` (read by POST, D-H-4).

---

## Phase graph

```
P1 storage ──► P2 server ──► G (coordinator: just openapi, just api-examples) ──► P3 web client ──► P4 web mocks and tests ──► P5 docs (coordinator)
```

All phases run in order in the one worktree. Edit lists do not overlap. **No phase compiles, tests or builds** (machine load): each phase writes its files and commits; the coordinator runs `just check` once at the end and a fix phase follows if needed.

---

## P1 · Storage: a batch of facets over one set of rows

**Goal:** replace `facet` with `facets`, which builds the list's rows once and computes 1–20 fields concurrently, each with its own result.

**Edit:** `crates/storage/src/reads/facets.rs`, `crates/storage/src/reads/mod.rs`, `crates/storage/src/lib.rs`. Nothing else.

**Types and signatures** (replace `FacetRequest` and `facet`; every other public item stays):

```rust
/// Most fields one facet batch asks.
pub const MAX_FACET_FIELDS: usize = 20;

/// One field of a facet batch.
#[derive(Debug, Clone, Copy)]
pub struct FacetField<'a> {
    /// `column` or `map['key']` (see D1 of the filtering plan).
    pub field: &'a str,
    /// Case-insensitive substring the value or key must contain. Ignored by histograms.
    pub query: Option<&'a str>,
    /// Most values or keys returned, `1..=`[`MAX_FACET_LIMIT`].
    pub limit: usize,
}

/// A facet batch: fields over the rows of one list (D-C-23).
#[derive(Debug, Clone, Copy)]
pub struct FacetsRequest<'a> {
    /// The list the facets describe.
    pub view: FacetView,
    /// Filter text over `view`. Blank means no filter.
    pub filter: Option<&'a str>,
    /// Full-text search. Traces and spans only; blank means no search.
    pub search: Option<&'a str>,
    /// The list's window and default exclusions.
    pub scope: ListScope,
    /// The fields, `1..=`[`MAX_FACET_FIELDS`].
    pub fields: &'a [FacetField<'a>],
    /// Read timeout of the whole batch.
    pub timeout: Duration,
}

/// The facets of `request`, one result per field in request order.
///
/// # Errors
///
/// The outer error fails the whole batch: [`RunError::InvalidRequest`] for 0 or more than
/// [`MAX_FACET_FIELDS`] fields, or `search` on sessions; [`RunError::Invalid`] for a bad filter
/// (all checked before any query runs); [`RunError::Timeout`] when building the rows passes
/// `timeout`; DataFusion failures while building the rows through [`classify`].
/// A field's own error is in its slot: [`RunError::InvalidRequest`] for a bad limit, `q` or
/// field; [`RunError::Timeout`] when the field passes the deadline; query failures.
pub async fn facets(
    ctx: &QueryContext,
    request: FacetsRequest<'_>,
) -> Result<Vec<Result<FacetData, RunError>>, RunError>
```

The private enum named `FacetField` today (the parsed field) is renamed `ParsedField` (all its uses in the file and its tests), so the public struct can take the name `FacetField`.

**Steps:**
1. Rename the private enum `FacetField` to `ParsedField` everywhere in `facets.rs` (functions `parse_field`, `value_expr`, `values`, `run` and the tests).
2. Add `MAX_FACET_FIELDS`, `FacetField` and `FacetsRequest` as above; delete `FacetRequest`.
3. Add a private `fn check_field<'a>(view: FacetView, field: &FacetField<'a>) -> Result<(ParsedField, Option<&'a str>, usize), RunError>` holding today's per-field checks of `facet`, in today's order: limit range (same message `limit must be between 1 and {MAX_FACET_LIMIT}`), then `q` trim and length (same message `q must be 1 to {MAX_FACET_QUERY} characters`), then `parse_field`. It returns the parsed field, the trimmed `q` and the limit.
4. Write `facets` (replacing `facet`), in this order:
   1. If `request.fields.len()` is not in `1..=MAX_FACET_FIELDS`, return `Err(RunError::InvalidRequest(format!("fields must hold 1 to {MAX_FACET_FIELDS} fields")))`.
   2. If the view is `Sessions` and `present(request.search)` is `Some`, return the existing `search is not supported for sessions` error. (This stays before the filter parse: test `sessions_check_search_before_the_filter`.)
   3. Parse the filter as today (`ctx.parse_filter(filter_view(view), text)?` when present).
   4. `let checked: Vec<Result<(ParsedField, Option<&str>, usize), RunError>> = request.fields.iter().map(|field| check_field(request.view, field)).collect();`
   5. If no entry of `checked` is `Ok`, return `Ok` of the errors in order, without building the rows (no query runs).
   6. `let deadline = tokio::time::Instant::now() + request.timeout;`
   7. Build the rows once under the deadline: `tokio::time::timeout_at(deadline, rows(ctx, &request, filter))`; an elapsed timeout is `Err(RunError::Timeout)` (outer), an inner error is returned as the outer error. `rows` is today's `match` at the top of `run`: `window_rows` for traces and spans, `session_rows(ctx, request.scope, filter)` for sessions.
   8. Change `window_rows` to take `(ctx, table, view: FilterView, scope: ListScope, search: Option<&str>, filter: Option<Filter>)` instead of `&FacetRequest`; its body is unchanged otherwise.
   9. For each `checked` entry, in order, make a future: an `Err` is returned as is; an `Ok((field, query, limit))` runs `tokio::time::timeout_at(deadline, one(ctx, frame.clone(), &field, query, limit))`, mapping an elapsed timeout to `Err(RunError::Timeout)`. `one` is today's `match field` at the end of `run` (`histogram` for `Number`, else `values`). Await them together with `futures::future::join_all` and return `Ok` of the results (same order).
   10. Delete `run` (its two halves became `rows` and `one`).
5. Re-export in `reads/mod.rs` and `lib.rs`: replace `FacetRequest` by `FacetField`, `FacetsRequest`, `MAX_FACET_FIELDS`, and `facet` by `facets`. Keep the alphabetical order of each `pub use` list.
6. Tests (in `facets.rs`'s `mod tests`). Keep every existing test body unchanged by adding, in the test module, a test-only single-field shape and adapter:
   ```rust
   /// One field asked alone, the shape the tests were written for.
   #[derive(Debug, Clone, Copy)]
   struct Ask<'a> {
       view: FacetView,
       field: &'a str,
       filter: Option<&'a str>,
       search: Option<&'a str>,
       scope: ListScope,
       query: Option<&'a str>,
       limit: usize,
   }

   /// `ask` as a batch of one; the batch's error or the field's.
   async fn facet(ctx: &QueryContext, ask: Ask<'_>) -> Result<FacetData, RunError> {
       let fields = [FacetField { field: ask.field, query: ask.query, limit: ask.limit }];
       let mut results = facets(
           ctx,
           FacetsRequest {
               view: ask.view,
               filter: ask.filter,
               search: ask.search,
               scope: ask.scope,
               fields: &fields,
               timeout: TIMEOUT,
           },
       )
       .await?;
       results.pop().expect("one result per field")
   }
   ```
   `request(view, field)` returns an `Ask` (same field values as today, without `timeout`). Rename uses of the private enum in `parse_field_accepts_columns_and_map_keys` and siblings to `ParsedField`.
7. New tests:
   - `batch_answers_each_field_in_order`: traces, window `hour()`, fields `status`, `duration_ms`, `metadata`: three `Ok`s; the first equals the `status` answer of `status_values_count_traces_of_the_window` (`[("ok", 3), ("error", 1), ("unset", 1)]`), the second has kind `Histogram`, the third kind `Keys`.
   - `one_bad_field_does_not_fail_the_others`: fields `status`, `input`, then `status` with `limit: 0`: slot 0 `Ok`, slots 1 and 2 `Err(RunError::InvalidRequest(_))`.
   - `duplicate_fields_are_answered_twice`: fields `status`, `status`: two equal `Ok`s.
   - `no_field_or_too_many_fail_the_batch`: 0 fields and `MAX_FACET_FIELDS + 1` fields (all `status`) are outer `Err(RunError::InvalidRequest(_))`.
   - `bad_filter_fails_the_batch_before_the_fields`: filter `error_count >` with field `input` is outer `Err(RunError::Invalid(_))`.
   - `only_bad_fields_run_no_query`: fields `input` and `output` with timeout `Duration::ZERO`: `Ok` of two `InvalidRequest` errors (with a zero timeout, building the rows would have been a `Timeout`, so this shows no query ran).
   - `zero_timeout_fails_the_batch`: field `status` with timeout `Duration::ZERO` is outer `Err(RunError::Timeout)`.

**Done when:** the files are written. Do not compile or test (machine load); the coordinator runs the checks at the end.

**Commit:** `Batch facets over one set of rows in storage`

---

## P2 · Server: `POST /facets`

**Goal:** replace the `GET` route with a `POST` that takes a `FacetsRequest` body and answers one result per field.

**Edit:** `crates/server/src/api/facets.rs`, `crates/server/src/routes/facets.rs`, `crates/server/src/api/openapi.rs`, `crates/server/src/api_examples.rs`, new `fixtures/api/requests/facets-traces.json`, `fixtures/api/requests/facets-spans.json`, `fixtures/api/requests/facets-sessions.json`. Nothing else (not `manifest.json`, not `responses/`, not `openapi.json`: the coordinator regenerates them).

**Types** (`api/facets.rs`; delete `FacetsParams` and the old `FacetsResponse`; keep `FacetView`, `FacetKind`, `FacetValue`, `FacetBucket`, `FacetHistogram` unchanged; update the module comment to `POST /api/v1/projects/{project}/facets`):

```rust
/// Body of `POST /api/v1/projects/{project}/facets`: fields over the rows of one list (D-C-23).
// Deserialize only, because `TimeParam` has no `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FacetsRequest {
    /// The list the facets describe.
    pub view: FacetView,
    /// Inclusive range start. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Exclusive range end. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Filter DSL expression over `view`. Blank means no filter.
    #[serde(default)]
    pub filter: Option<String>,
    /// Full-text search. `traces` and `spans` only. At most 1,024 bytes.
    #[serde(default)]
    pub search: Option<String>,
    /// Include traces from the reserved environments (`cetra-eval`, `cetra-playground`, `cetra-assistant`). Default false.
    #[serde(default)]
    pub include_eval: bool,
    /// Include experiment traces. Default false.
    #[serde(default)]
    pub include_experiments: bool,
    /// The fields, 1 to 20. Answered in this order.
    pub fields: Vec<FacetFieldRequest>,
}

/// One field of a facet batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FacetFieldRequest {
    /// `column` or `map['key']`. A bare map column facets its keys.
    pub field: String,
    /// Most values or keys returned (1 to 100, default 10). Ignored by histograms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 100, default = 10)]
    pub limit: Option<u32>,
    /// Case-insensitive substring the value or key must contain. 1 to 200 characters after
    /// trimming. Ignored by histograms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

/// The answer of a facet batch: one result per requested field, in request order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetsResponse {
    /// The list the facets describe.
    pub view: FacetView,
    /// One result per requested field, in request order.
    pub results: Vec<FacetResult>,
}

/// One field's result: exactly one of `facet` and `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetResult {
    /// The field, as requested.
    pub field: String,
    /// The facet, when the field was answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet: Option<Facet>,
    /// Why this field was not answered. The other fields are not affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Problem>,
}

/// One field's facet.
///
/// `values` and `more` are present for kinds `values` and `keys`. `histogram` is present for
/// kind `histogram` when at least one row has a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Facet {
    /// What the answer describes.
    pub kind: FacetKind,
    /// Rows counted.
    pub total: u64,
    /// Rows with no value: null, or an empty list or map.
    pub missing: u64,
    /// At most `limit` values or keys, by count descending then value ascending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<FacetValue>>,
    /// True when more distinct values matched than `limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<bool>,
    /// The histogram of a number field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram: Option<FacetHistogram>,
}
```

`Problem` is `crate::api::common::Problem` (`use super::common::{Problem, TimeParam};`).

**Steps (routes/facets.rs):**
1. Route: `method: Method::POST`, same path, `access: Access::Project { perm: ProjectPerm::Query, session_only: false }`, `documented: true`, `handler: post(post_facets).layer(DefaultBodyLimit::max(FACETS_BODY_LIMIT))` with `/// Largest facet batch body. const FACETS_BODY_LIMIT: usize = 64 * 1024;` (imports as in `routes/dashboards.rs`). Update the route's doc comment: "Facet batch route. Documented; a read by POST (D-H-4)."
2. Handler `post_facets(State(state), Extension(access), error::Json(body): error::Json<FacetsRequest>) -> Result<error::Json<FacetsResponse>, ApiError>`, in this order:
   1. `check_search(body.search.as_deref())?`;
   2. `let scope = list_scope(body.from, body.to, body.include_eval, body.include_experiments)?;`
   3. `let fields: Vec<storage::FacetField<'_>> = body.fields.iter().map(|ask| storage::FacetField { field: &ask.field, query: ask.q.as_deref(), limit: usize::try_from(ask.limit.unwrap_or(DEFAULT_LIMIT)).unwrap_or(usize::MAX) }).collect();`
   4. `let deadline = Deadline::start();` then `open_query(&state, access.project.org_id, access.project.id, None).await?`;
   5. `storage::facets(&ctx, storage::FacetsRequest { view: storage_view(body.view), filter: body.filter.as_deref(), search: body.search.as_deref(), scope, fields: &fields, timeout: deadline.remaining()? }).await?` — an outer error becomes the whole response's problem through `?` (`ApiError::Query`, so a bad filter is `invalid_filter` on field `filter`, as today);
   6. zip `body.fields` with the results, enumerated: `Ok(data)` → `FacetResult { field, facet: Some(facet(data)), error: None }`; `Err(error)` → `let error = ApiError::QueryAt { field: format!("fields[{index}]"), error }; error.log(); FacetResult { field, facet: None, error: Some(error.problem()) }`.
   7. Answer `FacetsResponse { view: body.view, results }`.
3. Rename `response` to `facet(data: storage::FacetData) -> Facet` (drop the `view` and `field` arguments; same mapping as today).
4. Tests: replace the `facets` helper with `async fn facets(app: &TestApp, seeded: &Seeded, body: &Json) -> (StatusCode, Json)` sending `Method::POST` to `/api/v1/projects/{id}/facets` with `.json(body)` and the cookie (as `routes/dashboards.rs` `post_query` does). The window is `"from": "2026-01-01T10:00:00Z", "to": "2026-01-01T11:00:00Z"`. Port each test:
   - `values_answer_matches_storage`: body `{view: traces, window, fields: [{field: status}]}`; `results[0].facet` has the same assertions as today (total 5, missing 0, more `Some(false)`, no histogram, values `[ok 3, error 1, unset 1]`); `response.view` is `Traces`; `results[0].field` is `"status"`.
   - `histogram_answer_shape`: `fields: [{field: duration_ms}]`; `results[0].facet` has no `values` and no `more` keys in the JSON; the rest as today.
   - `keys_answer_has_values_and_more`: `fields: [{field: metadata}]`; same assertions on `results[0].facet`.
   - `bad_field_is_a_field_error` (was `bad_field_is_400_bad_request`): `fields: [{field: status}, {field: input}]` → `200`; `results[0].facet` present; `results[1].error.code == "bad_request"`, `results[1].error.status == 400`, `results[1].facet` absent.
   - `bad_filter_is_400_invalid_filter`: body with `filter: "name = 'x' )"`, `fields: [{field: status}]` → `400`, `code == "invalid_filter"`.
   - `sessions_search_is_400`: `{view: sessions, search: refund, fields: [{field: user_id}]}` → `400 bad_request`.
   - New `empty_or_too_many_fields_is_400`: `fields: []` → `400 bad_request`; 21 × `{field: status}` → `400 bad_request`.
   - New `unknown_member_is_400`: `fields: [{field: status, size: 3}]` → `400 bad_request`.
   - New `get_is_not_a_route`: `GET /api/v1/projects/{id}/facets?view=traces&field=status` with the cookie → status is `405` (Method Not Allowed). If the router answers another status, stop and report it.
   - `a_member_without_project_view_gets_404` and `no_cookie_is_unauthorized`: same, with `POST` and body `{view: traces, fields: [{field: status}]}`.
5. `api/openapi.rs`: replace the `get_facets` operation by `post_facets` (`operation("post_facets", "traces", "Get facets")`), with `.request_body(Some(json_body("FacetsRequest")))`, `.parameters(Some(vec![project_param()]))` (no query parameters), responses `json_of::<FacetsResponse>("One result per field, in request order.")` and errors `&[400, 401, 403, 404, 413, 415, 422, 429, 503]`; the path item is `PathItem::new(HttpMethod::Post, facets)`. Description (replace the old one; keep `{EXCLUSIONS}` first as today):
   "{EXCLUSIONS} Answers 1 to 20 fields over the rows the list would show: the window, the default exclusions, `search` and `filter`, given once for every field, but not the list's cursor, sort or limit. The answer has one result per field, in request order, with either `facet` or `error`: a bad field, `limit` or `q`, or a field that times out, fails only that field. A bad filter, `search` on sessions, a bad window, or 0 or more than 20 fields fail the whole request. A field is a column of the view, or `map['key']` of a map column; a bare map column facets its keys. `kind` is `values` (strings, booleans, lists and `map['key']`), `keys` (a bare map) or `histogram` (numbers: min, max, approximate p50, p95 and p99, and 20 equal buckets below p99 plus one bucket from p99 to max). `total` counts rows, `missing` the rows with no value (null, or an empty list or map). Values are counted per row: a list counts a row once under each distinct element. Keys starting with `cetra.` are left out. `q` keeps values containing it, ignoring case. Spans count stored copies separately. Sessions take no `search`. Nothing is cached on the server. A read: a read-only personal access token and a `read` key may call it."
   Schemas: next to `set.insert::<FacetsResponse>();` (around line 365) add `set.insert::<FacetsRequest>();`, `set.insert::<FacetFieldRequest>();`, `set.insert::<FacetResult>();` and `set.insert::<Facet>();`, and fix the `use super::facets::{…}` import (line ~102): `FacetsParams` goes, the four new names come. The `with_query::<FacetsParams>` parameters go (replaced by `project_param()` alone).
6. `api_examples.rs`: replace the per-field `FACETS` loop by one batch per view. Delete `FACET_WINDOW` and `FACETS`. For each view in `["traces", "spans", "sessions"]`: `let request: Value = read_json(&api_dir().join(format!("requests/facets-{view}.json")));`, `let facets: FacetsResponse = post_json(&app, cookie, "/facets", &request).await;`, assert every result has a `facet` (`assert!(facets.results.iter().all(|result| result.facet.is_some()), "{view}")`), then `out.add("post_facets", &[], &[], &format!("responses/facets-{view}.json"), &facets);`. Keep the comment "Every facet the web shows open by default, and every preset field, has an answer here." above the loop.
7. Request files (hand-written, two-space JSON, trailing newline). Each has `view`, `"from": "2026-01-01T00:00:00Z"`, `"to": "2026-01-02T00:00:00Z"` and `fields`, each field `{ "field": "<name>" }` with no `limit` and no `q`:
   - `facets-traces.json`: `status`, `name`, `environment`, `tags`, `models`, `metadata`, `duration_ms`, `cost_total`.
   - `facets-spans.json`: `span_kind`, `status`, `name`, `model`, `duration_ms`, `cost_total`.
   - `facets-sessions.json`: `user_id`, `tags`, `duration_ms`, `cost_total`.

**Done when:** the files are written. Do not compile or test; the coordinator runs the checks.

**Commit:** `Replace GET /facets with a batched POST /facets`

---

## G · Coordinator: regenerate

Not an agent phase. The coordinator runs `just openapi` (writes `crates/server/openapi.json`, `web/src/api/schema.gen.ts`) and the API examples recipe (`just api-examples`: writes `fixtures/api/manifest.json` and `fixtures/api/responses/facets-{traces,spans,sessions}.json`, and the old `responses/facets-<view>-<field>.json` files are deleted by hand with `git rm` if the recipe leaves them), checks the diff, and commits `Regenerate the OpenAPI document and API examples for POST /facets`.

---

## P3 · Web client: per-field queries through a batcher

**Goal:** `facetQuery` and `useFacetQuery` keep their signatures and behaviour, but fetch through a batcher that sends one `POST /facets` per shared part.

**Edit:** `web/src/api/queries.ts`, new `web/src/api/facet-batch.ts`, `web/src/api/queries.test.tsx`, new `web/src/api/facet-batch.test.ts`, `web/src/components/facets/preset-chips.tsx`, and any of `web/src/components/facets/{values-facet,range-facet,keys-facet}.tsx`, `web/src/components/filter-bar/{editor,value-input}.tsx` **only if** they read `.view` or `.field` off the answer (the answer type becomes `Schema<"Facet">`, which has neither). Nothing else.

**`web/src/api/facet-batch.ts`** (new; write it as given, comments included):

```ts
import { api } from "./client";
import type { Schema } from "./types";

/**
 * Fields asked within this many milliseconds share one `POST /facets` (D-C-23). One frame is
 * enough for the sections and preset chips a page mounts together.
 */
export const FACET_BATCH_MS = 10;

/** What every field of one batch shares: the list, its window, flags, filter and search. */
export type FacetShared = Omit<Schema<"FacetsRequest">, "fields">;

type Waiting = {
  field: Schema<"FacetFieldRequest">;
  resolve: (facet: Schema<"Facet">) => void;
  reject: (error: unknown) => void;
};

type Batch = { project: string; shared: FacetShared; waiting: Waiting[] };

const open = new Map<string, Batch>();

/**
 * One field's facet. Fields asked with the same project and shared part within
 * `FACET_BATCH_MS` go out as one request; each gets its own result or error back. A field's
 * error, or the whole request's, is thrown as the problem document, as `$api` queries throw it.
 */
export function askFacet(
  project: string,
  shared: FacetShared,
  field: Schema<"FacetFieldRequest">,
): Promise<Schema<"Facet">> {
  const key = JSON.stringify([project, shared]);
  return new Promise((resolve, reject) => {
    let batch = open.get(key);
    if (batch === undefined) {
      const created: Batch = { project, shared, waiting: [] };
      open.set(key, created);
      setTimeout(() => {
        open.delete(key);
        void send(created);
      }, FACET_BATCH_MS);
      batch = created;
    }
    batch.waiting.push({ field, resolve, reject });
  });
}

async function send(batch: Batch): Promise<void> {
  // The same field asked twice (a `fetchQuery` racing a `useQuery`) is sent once.
  const fields: Schema<"FacetFieldRequest">[] = [];
  const slots = batch.waiting.map(({ field }) => {
    const text = JSON.stringify(field);
    const found = fields.findIndex((known) => JSON.stringify(known) === text);
    if (found !== -1) {
      return found;
    }
    fields.push(field);
    return fields.length - 1;
  });
  let answer: Schema<"FacetsResponse"> | undefined;
  try {
    const { data, error } = await api.POST("/api/v1/projects/{project}/facets", {
      params: { path: { project: batch.project } },
      body: { ...batch.shared, fields },
    });
    if (error !== undefined || data === undefined) {
      throw error ?? new Error("facets: empty answer");
    }
    answer = data;
  } catch (error) {
    for (const waiting of batch.waiting) {
      waiting.reject(error);
    }
    return;
  }
  batch.waiting.forEach((waiting, index) => {
    const result = answer.results[slots[index] ?? -1];
    if (result?.facet !== undefined) {
      waiting.resolve(result.facet);
    } else {
      waiting.reject(result?.error ?? new Error("facets: missing result"));
    }
  });
}
```

If `api.POST`'s inferred types reject this shape (for example `error` typed `never`), stop and report the compiler message instead of casting.

**`web/src/api/queries.ts` steps:**
1. Keep `FacetAsk` unchanged.
2. Add (exported, for the tests):
   ```ts
   /** The shared part of a facet ask, as the request body spells it, with empty parts left out. */
   export function facetShared(query: FacetAsk): FacetShared {
     return {
       view: query.view,
       from: query.from,
       to: query.to,
       ...(query.filter ? { filter: query.filter } : {}),
       ...(query.search ? { search: query.search } : {}),
       ...(query.includeEval ? { include_eval: true } : {}),
       ...(query.includeExperiments ? { include_experiments: true } : {}),
     };
   }

   /** The field part of a facet ask, with empty parts left out. */
   export function facetField(query: FacetAsk): Schema<"FacetFieldRequest"> {
     return {
       field: query.field,
       ...(query.q ? { q: query.q } : {}),
       ...(query.limit === undefined ? {} : { limit: query.limit }),
     };
   }
   ```
3. Rewrite `facetQuery`:
   ```ts
   export function facetQuery(org: string, project: string, query: FacetAsk) {
     const ref = projectRef(org, project);
     const shared = facetShared(query);
     const field = facetField(query);
     return queryOptions({
       queryKey: ["facets", ref, shared, field] as const,
       queryFn: () => askFacet(ref, shared, field),
       staleTime: 30_000,
       // Keeps the previous answer on screen while a filter, q or limit change refetches.
       placeholderData: keepPreviousData,
     });
   }
   ```
   (`queryOptions` from `@tanstack/react-query`; import `askFacet` and `FacetShared` from `./facet-batch`.) Update the doc comment above `FacetAsk`: "One facet of a list (D1). Each field has its own cache entry, kept 30 seconds; fields asked together go out as one `POST /facets` (D-C-23). The server keeps none."
4. `type FacetAnswer = Schema<"Facet">;`. `FacetKey` stays `ReturnType<typeof facetQuery>["queryKey"]`.
5. Rewrite `latestFacetAnswer(client, key)` for the new key, with the same rule: `const [, ref, shared, field] = key;`, `const { from, to, ...rest } = shared;`, length from `windowLength(from, to)` (return `undefined` when undefined), `const same = JSON.stringify([rest, field]);`, then scan `client.getQueryCache().findAll({ queryKey: ["facets", ref] })`; for each entry with data, read `const [, , cachedShared, cachedField] = cached.queryKey as FacetKey;`, skip unless `windowLength(cachedShared.from, cachedShared.to) === length` and `JSON.stringify([cachedRest, cachedField]) === same` (where `cachedRest` is `cachedShared` without `from`/`to`); keep the newest `dataUpdatedAt`. `useFacetQuery` stays as it is.
6. `preset-chips.tsx`: remove `limit: 1` from its `useFacetQuery` ask (a histogram ignores `limit`; without it the key equals the range facet's when the filter is empty, so both share one entry). Nothing else changes there.
7. Callers (`values-facet`, `range-facet`, `keys-facet`, `editor`, `value-input`): they keep calling `facetQuery`/`useFacetQuery` as today. Only if one reads `.view` or `.field` off the answer, replace that read with the value it asked for. If a caller needs anything else, stop and report.

**Tests:**
- `web/src/api/facet-batch.test.ts` (MSW `server.use(http.post("*/api/v1/projects/:project/facets", …))`, record each request body):
  - `fields asked together go out as one request`: three `askFacet` calls with the same project and shared part, different fields, without awaiting between them → one request whose `fields` are the three in call order; each promise resolves to its own `facet`.
  - `a field error rejects only that field`: the handler answers `error` (a problem with `code: "bad_request"`, `status: 400`) for the second field → the first and third resolve, the second rejects with that problem object.
  - `a whole-request problem rejects every field`: the handler answers `400` with a problem body → every promise rejects with an object whose `code` is the problem's code (check with `problemOf` from `./problem`).
  - `different shared parts go out separately`: two calls with different `filter` → two requests, one field each.
  - `the same field asked twice is sent once`: two calls with equal shared part and field → one request with one field; both promises resolve to the same facet.
  - `a later ask starts a new batch`: one call, await it, then another → two requests.
- `web/src/api/queries.test.tsx`: update the cached-key helpers to the new key (they use `facetQuery(...).queryKey`, so mostly unchanged) and the `useFacetQuery` test's handler from `http.get` to `http.post` answering `{ view, results: body.fields.map((field) => ({ field: field.field, facet })) }`. Keep every existing case: same-length window found, different length not found, other project not found, placeholder shown while the new window loads.
- Add in `queries.test.tsx`: `facetQuery keys leave out empty parts`: `facetQuery("acme", "support-bot", { view: "traces", field: "status", from, to, filter: "", search: "", includeEval: false, q: "", limit: 10 }).queryKey` equals `["facets", <ref>, { view: "traces", from, to }, { field: "status", limit: 10 }]`, where `<ref>` is `projectRef("acme", "support-bot")`.

**Done when:** the files are written. Do not run tests, type checks or builds; the coordinator runs the checks.

**Commit:** `Batch facet requests in the web client`

---

## P4 · Web mocks and page tests

**Goal:** every MSW mock and test that served `GET /facets` serves `POST /facets` instead.

**Edit:** `web/src/mocks/handlers.ts`, `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts`, new `web/src/test/facets.ts`, `web/src/components/facets/facet-panel.test.tsx`, `web/src/components/facets/preset-chips.test.tsx`, `web/src/components/facets/range-facet.test.tsx`, `web/src/components/filter-bar/filter-bar.test.tsx`, `web/src/components/saved-views/default-view.test.tsx`, `web/src/routes/$org/$project/traces/index.test.tsx`. Nothing else.

**Steps:**
1. `web/src/mocks/handlers.ts`: next to `widgetQueryFixture`, add `facetsFixture = http.post("/api/v1/projects/:project/facets", …)` and put it in `handlers` right after `widgetQueryFixture`. It imports `responses/facets-traces.json`, `facets-spans.json` and `facets-sessions.json` (as the widget fixtures are imported), reads the body as `Schema<"FacetsRequest">`, picks the file of `body.view`, and answers `{ view: body.view, results: body.fields.map(...) }` where each field's result is the fixture result with the same `field` (ignoring `limit` and `q`), or, when the fixture has none, `{ field, error: { type: "about:blank", title: "Not Found", status: 404, code: "not_found" } }`. Doc comment: "The facet batch answers each field from the per-view examples (`requests/facets-<view>.json`); a field with no example gets a `not_found` error. The manifest serves GET only, so this POST is answered here."
2. `web/src/mocks/generated/handlers.ts`: change the route to `http.post(\`${PROJECT}/facets\`, ({ request }) => facets(dataset, request))` and make `facets` `async`, reading the body:
   1. `const body = (await request.json()) as Partial<Schema<"FacetsRequest">>;`
   2. Whole-request checks, each answered with the existing `problem(...)` helper and status 400, in this order: `view` is not `traces`/`spans`/`sessions` (`{ field: "view", message: "is not a view" }`); `fields` is not an array of 1 to 20 entries (`{ field: "fields", message: "must hold 1 to 20 fields" }`); `view === "sessions"` and `body.search` is a non-empty string (`{ field: "search", message: "is not supported for sessions" }`).
   3. The window, flags, filter and search: build `const url = new URL(request.url);` and, for each of `from`, `to`, `filter`, `search`, `include_eval` and `include_experiments` present in the body, `url.searchParams.set(name, String(value))`; then call the existing `readQuery(url)` and return its `Response` when it is one (a `filter` of `bad` stays the mock's `invalid_filter`). `readQuery` also reads `limit`, `cursor`, `sort` and `order`; leave them unset so their defaults apply (the field's `limit` is checked per field below, not by `readQuery`).
   4. `const rows = facetRows(dataset, view, parsed);` once.
   5. For each field, in order, produce a `Schema<"FacetResult">`: the existing per-field checks (field, limit, q, same messages) become `{ field: ask.field, error: { type: "about:blank", title: "Bad Request", status: 400, code: "bad_request", errors: [{ field: "fields[<index>]", message }] } }`; a good field is `{ field: ask.field, facet }` where `facet` is today's `facetAnswer(view, field, rows, q, limit)` without its `view` and `field` members (change `facetAnswer` to return `Schema<"Facet">`).
   6. Answer `HttpResponse.json({ view, results })`. Update the doc comment to `POST …/facets` (D-C-23).
3. `web/src/mocks/generated/handlers.test.ts`: port the six facet tests (lines ~405–520) to `fetch(\`${PROJECT}/facets\`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ view, fields: [...] }) })`, reading `results[0].facet` where they read the answer. The bad-request test(s) at ~513: a bad field/limit/q is now `200` with `results[0].error.code === "bad_request"`; a bad view stays a `400`. Add `answers every field of a batch in order` (traces, fields `status` and `duration_ms`: two results, `values` then `histogram` kinds).
4. `web/src/test/facets.ts` (new), a helper for page tests:
   ```ts
   import { HttpResponse, http } from "msw";
   import type { Schema } from "@/api/types";

   /** The answer of one field, or undefined for an empty values facet. */
   export type FacetAnswerer = (
     field: Schema<"FacetFieldRequest">,
     body: Schema<"FacetsRequest">,
   ) => Schema<"Facet"> | undefined;

   const EMPTY: Schema<"Facet"> = { kind: "values", total: 0, missing: 0, values: [], more: false };

   /** A `POST /facets` handler answering each field with `answer`. */
   export function facetsHandler(answer: FacetAnswerer) {
     return http.post("*/api/v1/projects/:project/facets", async ({ request }) => {
       const body = (await request.json()) as Schema<"FacetsRequest">;
       return HttpResponse.json({
         view: body.view,
         results: body.fields.map((field) => ({ field: field.field, facet: answer(field, body) ?? EMPTY })),
       });
     });
   }
   ```
   If `web/src/test/` has an index or a naming rule this does not follow, stop and report.
5. Port each test file's `http.get(".../facets", …)` overrides to `facetsHandler` (or, for handlers that delay or count requests, to `http.post` reading the body). Where a test reads `url.searchParams.get("field")`, `"limit"`, `"q"`, `"filter"` or `"view"`, read the field's `field`, `limit`, `q` or the body's `filter`/`view` instead. Where a test records facet requests as URLs (`recordFacets` in `facet-panel.test.tsx`, `default-view.test.tsx` checking `field === "name"`), record the parsed bodies instead (`await request.clone().json()` in the `request:start` listener) and assert on the fields. Keep each test's intent and assertions; change only how requests are read.
6. New test in `facet-panel.test.tsx`: `opening the spans page sends one facet request`: navigate to the spans page with no filter, wait for the `Status` section's values, then assert exactly one `/facets` request was recorded and that its `fields` hold `span_kind`, `status`, `name` and `duration_ms` (any order) and `cost_total`. Then open one more section (any collapsed value section of the spans facets) and assert exactly one more request, whose `fields` is that one field. If the spans page's default open sections are not those named here, use the ones the page opens and report the difference.

**Done when:** the files are written. Do not run tests, type checks or builds; the coordinator runs the checks.

**Commit:** `Serve POST /facets in the MSW mocks and page tests`

---

## P5 · Coordinator: docs

Not an agent phase; the coordinator writes it.
- `docs/spec/05-query.md` § Facets: the request line becomes `POST /api/v1/projects/{p}/facets` with the body (`view`, `from`, `to`, `filter`, `search`, `include_eval`, `include_experiments`, `fields: [{ field, limit?, q? }]`, 1–20 fields) and the answer (`results` in request order, each `facet` or `error`); which errors fail the whole request and which only a field; the storage builds the rows once and computes the fields concurrently under one deadline; the cost paragraph: "the UI fetches only expanded facets, keeps each answer 30 s, and sends the fields it asks together as one request".
- `docs/spec/09-api.md`: the `GET /facets` row becomes `POST /facets`, a read by POST (D-H-4).
- `docs/spec/11-ui.md`: `GET /facets` → `POST /facets` (filter bar, presets); the facet sidebar paragraph adds: "Each section keeps its own cached answer; the fields asked together (a page opening, the presets) go out as one request per distinct filter (D-C-23)."
- `docs/mvp-decisions.md`: D-C-13 says "one field per request" → "one request per batch of fields (D-C-23)"; D129 `GET /facets` → `POST /facets`; add D-C-23 (the text of the table row above).
- `CHANGELOG.md` § `[Unreleased]` › `Changed`: "`GET /facets` is replaced by `POST /facets`, which answers up to 20 fields in one request; the facet sidebar and presets send one request when a list opens (D-C-23)." and update the `Added` line that says "Backed by the new `GET /facets` endpoint" to `POST /facets`.
- `website/`: no page names the endpoint (checked: `grep -rn "/facets" website/src` is empty).

Commit: `Document the facet batch (D-C-23)`.

## Final checks (coordinator)

1. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`; `just check`.
2. In the browser (`just web-dev`, mocked API): open Traces, Spans, Sessions and Users; each sends one `POST /facets` on open (network panel), and opening a section or typing in a section's value search sends one request with only that field.
3. Sonnet code review of the whole branch; fixes by a Haiku phase with an exact brief.
4. Open the PR. Do not merge.
