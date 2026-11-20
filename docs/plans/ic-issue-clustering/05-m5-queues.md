# IC M5 · Queues — plan

M5 connects issues to annotation queues (D-IC-14, "Send to queue", made exact by D-IC-27 to D-IC-31):
- an issue page gets a **Send to queue** button that adds the issue's newest occurrences to a queue of any kind (trace, span or session);
- a new route, `POST /api/v1/projects/{project}/issues/{id}/queue-items`, does it through the add-by-id path of track AQ, never a copy of it;
- the AQ add route is refactored so both routes share one core (the way M4.11 extracted `add_trace_refs`).

**Prerequisites:** M1 to M4 and track AQ are on `main`, which this branch is cut from. AQ's tables exist (migration `0025_annotation_queues.sql`, unchanged); M4's migration is `0030`.

**No migration.** M5 adds no table, column or index: it reads occurrence scores (storage) and writes `annotation_queue_items` through AQ's existing insert. The latest migration stays `0030`; no schema dump, `meta_schema.snap` or spec 04 text changes. (A queue item has no metadata column, so an item cannot record which issue it came from; that would need a migration and an AQ change, and is not part of M5: Q6.)

**Read first:**
- `AGENTS.md`;
- `00-design.md` (D-IC-9, D-IC-11, D-IC-14, D-IC-18, D-IC-22) and the decisions this plan adds (Appendix A, copied into the design doc by M5.1);
- `04-m4-links.md` § M4.10a and § M4.11 (the template of this plan: `example_issues`, `occurrence_scores`, the dataset route) and its coordinator answers Q2 and Q3;
- track AQ: `docs/plans/aq-annotation-queues/00-design.md`; D-AQ-2, D-AQ-8, D-AQ-9, D-AQ-11 in `docs/mvp-decisions.md`; `crates/server/src/routes/annotation_queues.rs` (`add_items`, `insert_found`, `parse_target`, `shape_error`, `items_answer`, `skip`, `added_by_of`, the `add_items_*` tests), `meta/annotation_queues.rs` (`insert_items_tx`, `MAX_ITEMS_PER_QUEUE`, `get`), `api/annotation_queues.rs` (`QueueItemTarget`, `AddedQueueItems`, `SkippedQueueItem`);
- the M4 code M5 mirrors: `routes/issue_dataset_items.rs`, `issues/read.rs` (`example_issues`, `occurrence_scores`, `OCCURRENCE_DEFAULT_PERIOD_NS`), `routes/datasets.rs` (`add_trace_refs`), `test_support/issues.rs`;
- web: `web/src/components/annotation-queues/add-to-queue-dialog.tsx` (+ test), `web/src/api/queries-annotation-queues.ts`, `components/issues/issue-actions.tsx`, `components/datasets/add-traces-to-dataset-dialog.tsx` (`AddIssueExamplesDialog`, `DatasetTargetDialog`), `routes/$org/$project/issues/$issueId.tsx` (+ test), `mocks/writes.ts`, `lib/roles.ts` (`canManageQueues`).

## Open questions for the coordinator

The user's answers fix most of M5 (D-IC-27 to D-IC-31). These points are not settled by the code or the brief. Each phase is written with the **assumption** shown; change the assumption and the phase text together.

| # | Question | Assumed | Phase |
|---|---|---|---|
| Q1 | Is `queue` an id or a name? | **An id (UUID).** Every AQ route takes the queue id (`QueuePath { id: Uuid }`); AQ has no by-name lookup anywhere, queue names are editable (`PATCH`) so a name in an integration breaks silently, and the web picks from a list that carries ids. J's datasets take an id or a name because J's routes do. A body whose `queue` is not a UUID is a 400 (serde), an unknown id a 404 | M5.1, M5.3a, M5.3b |
| Q2 | The group has occurrences, but none fits the queue's kind (all lack a span or a session) | **`201` with `added: 0` and every occurrence in `skipped`**, as AQ's add answers 201 whatever the counts. The 400 `issue` "the issue has no occurrences in the data window" is only for a group with no occurrence at all (Q3 of the M4 plan). The dataset route differs (its core answers 400 when everything is skipped); AQ's shape is the one the web reuses | M5.3b |
| Q3 | What does `skipped[].index` mean, there being no request list? | **The position of the occurrence in the group's occurrences read newest first** (0-based, before the per-target dedupe), the scan stopping once `limit` targets are found. The AQ core's own skips (`the trace was not found`, `a deletion blocks this target`, …) use the index of the target's newest occurrence. A duplicate occurrence of an already-selected target is dropped silently (not skipped) | M5.3b |
| Q4 | Rows read per issue | **`limit` per issue (at most 1 000, the storage page cap), 8 issues at most**, as M4.11 reads `limit` rows per name; session and span dedupe can leave fewer than `limit` targets although more exist (accepted, as in M4). Worst case the `skipped` list holds 8 000 short entries (accepted; no cap) | M5.3b |
| Q5 | Does the dialog have a count field? | **No**: it sends no `limit`, so the default 100 applies, as `AddIssueExamplesDialog` does. The API takes 1 to 1 000. (The site's "Add examples to a dataset" text says the dialog has a count field; it does not. M5.6 fixes that sentence.) | M5.4, M5.6 |
| Q6 | Should a queue item record the issue it came from (like the dataset item's `cetra.issue_id`)? | **No.** `annotation_queue_items` has no metadata column; D-IC-31 says so. The issue page does not list its queue items | M5.1 |
| Q7 | Does the queue picker offer "New queue…"? | **Yes**, as AQ's add dialog does (the `QueueDialog` starts on the `trace` kind and the user may change it) | M5.4 |
| Q8 | An occurrence whose target is a span (`ScoreTarget::Span`) or a session (`ScoreTarget::Session`, a feedback issue) | Trace queue: a span occurrence gives its trace; a session occurrence is skipped ("the occurrence has no trace"). Span queue: a span occurrence gives its span; a trace occurrence gives the `cetra.issue.span_id` of its metadata; else skipped ("the occurrence has no span"). Session queue: a session occurrence gives its session; a trace or span occurrence gives its `session_id`; else skipped ("the occurrence has no session") | M5.3b |

**Coordinator's answers (binding):** Q1 to Q8 as assumed. Q2 keeps AQ's "201 whatever the counts" when the group has occurrences but none fits the queue's kind; the `400` on `issue` is only for a group with no occurrence in the window. Q6: no issue reference on queue items in M5 (it would need an AQ migration); revisit only if the user asks.

## What it is, and what it is not

When M5 is done:
- the issue page of a non-merged issue shows **Send to queue** to members and above (`canManageQueues`); a dialog lists the project's queues with their kind, and shows the add result with the skipped list;
- `POST …/issues/{id}/queue-items` adds up to `limit` (default 100, at most 1 000) occurrence targets of the issue group to the chosen queue, through AQ's add path (data window, deletion guard re-checked in the insert transaction, item cap, one item per target, `added_by`);
- AQ's `add_items` and the new route share one core, so no check is written twice.

Out of scope:
- a bulk "Send to queue" on the issue list (D-IC-27);
- a count field in the dialog (Q5), a record of the issue on the queue item (Q6);
- the MCP server and the SDKs (no new tool or client method);
- the CLI e2e (the route is covered in-process, as AQ's add is; M4's e2e covers the real binary for datasets only);
- any change to AQ's API shapes, permissions or limits.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IC-9, D-IC-11, D-IC-18 | Merge sets of names; occurrence scores (`issue:<id>`, `cetra.issue.span_id`, the trace's `session_id`, `created_at` = occurrence time) |
| D-IC-14, D-IC-22 | Links; the dataset route M5 mirrors (group of 8 names, 14-day window, newest first) |
| **D-IC-27 to D-IC-31** | This plan's decisions (Appendix A, copied into the design doc and the decision log by M5.1) |
| D-AQ-2, D-AQ-8, D-AQ-9, D-AQ-11 | One kind per queue; add by id (1–1 000 targets, data window, guard re-check, 100 000 cap, skipped shape); items follow their target; `ManageQueues` |
| D-J-25 | The bulk-add core pattern M4.11 followed |

## Versions

No new dependency. (Rust: `uuid`, `serde`, `axum`, `utoipa` are in use; web: no new package.)

## Phases

| Phase | Edits (summary) | Depends on |
|---|---|---|
| M5.1 Decisions | `00-design.md`, `docs/mvp-decisions.md` | — |
| M5.2 AQ add core | `routes/annotation_queues.rs` | — |
| M5.3a Request type and OpenAPI | `api/annotation_queues.rs`, `api/openapi.rs`, `tests/openapi.rs` | — |
| M5.3b Issue queue route | new `routes/issue_queue_items.rs`, `routes/mod.rs`, `http/access.rs` | M5.2, M5.3a |
| M5.4 Web: shared queue dialog and the issue dialog | `add-to-queue-dialog.tsx` | — |
| M5.5 Web: issue page button | `issue-actions.tsx`, `$issueId.tsx`, `$issueId.test.tsx`, `mocks/writes.ts` | M5.4 |
| M5.6 Docs | specs 06, 09, 11, website pages, `CHANGELOG.md`, `docs/roadmap.md` | M5.1 |
| Closing | generated files, checks | all |

**Parallelism** (the Edit lists are disjoint):
- Start together: **M5.1, M5.2, M5.3a, M5.4**.
- After M5.1: **M5.6**. After M5.2 and M5.3a: **M5.3b**. After M5.4: **M5.5**.
- Then the Closing section.
- **M5.2 leaves the crate compiling and its behaviour unchanged** (a pure extraction; AQ's tests are its proof). **M5.3a** adds a request type, an OpenAPI operation that names `IssueQueueItemsRequest` and a test table row; the OpenAPI operation needs the route's handler registered to pass `tests/openapi.rs`, so the crate's tests are only green after M5.3b (the Closing runs them once). M5.3a compiles on its own: the operation builder only references types.
- Web phases never need the Rust phases to compile: they are written against the contract in M5.3a (`post_issue_queue_items`). `web/src/api/schema.gen.ts` is regenerated in the Closing, so M5.4 and M5.5 type-check only after it; the plan's code uses the operation path as a string literal, as M4.13 did.

**Rules for every phase** (repeat them in each brief):
- Follow the plan exactly. If a step is unclear, contradicts the code or cannot be done as written, stop and report instead of improvising.
- Touch only the phase's **Edit** files.
- **No compiling or testing during the phases** (the user's rule: the machine is overloaded). The only commands allowed are `rustfmt --edition 2024 <the .rs files you edited>` and, for web files, `pnpm --dir web exec biome format --write <the files you edited>`. Write each test with its phase. Every check runs once, in the Closing section.
- Do not regenerate generated files (`openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`, `fixtures/api/static/*` that tests regenerate, insta snapshots, `website/src/generated/*`, `web/src/mocks/generated/*`, docs-gen pages). They are regenerated at the end.
- No `unwrap()`/`expect()` outside tests unless a comment states the invariant. No payload text (titles, summaries, session ids, span or trace content) in logs or error messages: log ids and counts only. A skip message never contains the rejected value (AQ's rule).
- **Dead code:** M5 adds none: every new `pub(crate)` item has its caller inside the same or an earlier phase. If a `pub(crate)` item still has no non-test caller when you finish, stop and report.
- Re-read the current text of each file before editing it; line numbers are only hints, names and signatures are exact.
- Commit once, with the given subject, staging only the phase's files (`git add <paths>`).

---

## M5.1 Decisions

**Goal:** the decisions of D-IC-27 to D-IC-31 are in the design doc and the decision log.

**Edit:** `docs/plans/ic-issue-clustering/00-design.md`; `docs/mvp-decisions.md`.

**Steps:**
1. `00-design.md`:
   - after the `### D-IC-25 End-to-end coverage` section (before the next `##` heading) add the five sections of Appendix A (`### D-IC-27 …` to `### D-IC-31 …`);
   - in `### D-IC-14 Links`, replace the **Annotation queues** bullet by: "**Annotation queues** (track AQ): "Send to queue" on an issue adds its newest occurrences to a queue through AQ's add-by-id path (D-IC-27 to D-IC-31).";
   - in the milestones table, the M5 row becomes: **M5** Queues · "Send to queue" on the issue page: `POST …/issues/{id}/queue-items`, the shared AQ add core, the dialog · M4; AQ (done) · Bulk send from the list, a count field in the dialog, a record of the issue on the item; replace "M5 is a small plan written when AQ has landed." by "M5 has its own plan (`05-m5-queues.md`)."
2. `docs/mvp-decisions.md` § Track IC: after the `D-IC-26` row add the five rows of Appendix A (the one-line forms), and in the `D-IC-14` row replace `"Send to queue" waits for track AQ's API.` by `"Send to queue" is D-IC-27 to D-IC-31.`.

**Tests:** none.

**Done when:** `grep -n "waits for track AQ" docs/mvp-decisions.md` finds nothing, and `grep -n "in the milestone after AQ's API lands" docs/plans/ic-issue-clustering/00-design.md` finds nothing (the old D-IC-14 bullet text is gone); the ids D-IC-27 to D-IC-31 appear in both files.

**Commit:** `Record the IC M5 decisions`

---

## M5.2 Extract AQ's add core

**Goal:** the part of `add_items` after the format checks becomes one `pub(crate)` function the issue route calls, so AQ's checks exist once (D-IC-31). No behaviour change.

**Edit:** `crates/server/src/routes/annotation_queues.rs` only (including its tests, only if a name they use changes visibility: none should).

**Contract:**
```rust
/// One target of an add after the format checks. `index` is its position in the request
/// (for the issue route: in the occurrences read, D-IC-29).
pub(crate) struct Parsed {
    pub(crate) index: usize,
    pub(crate) trace: Option<TraceId>,
    pub(crate) span: Option<SpanId>,
    pub(crate) session: Option<String>,
}

/// Longest session id, in bytes (was private).
pub(crate) const MAX_SESSION_ID_BYTES: usize = 255;

pub(crate) fn skip(index: usize, message: &str) -> SkippedQueueItem
pub(crate) fn added_by_of(principal: &Principal) -> Result<(store::AddedByKind, Uuid), ApiError>

/// The core of both adds by id (D-AQ-8): looks `fitting` up in storage over the data window
/// (a trace, a trace and span, or a session, by the queue's `target`), lists what is missing in
/// `skipped`, then guards and inserts the rest in the writer's transaction (`insert_found`: the
/// queue is checked again, the deletion guard is asked per target, a queue past 100 000 items is
/// 422 and nothing is added). `fitting` holds only targets that fit `target`; `skipped` holds the
/// skips the caller already made. Returns the answer with `skipped` sorted by `index`, and logs
/// ids and counts.
pub(crate) async fn add_found_targets(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    queue_id: Uuid,
    target: Target,
    added_by: (store::AddedByKind, Uuid),
    fitting: Vec<Parsed>,
    mut skipped: Vec<SkippedQueueItem>,
) -> Result<AddedQueueItems, ApiError>
```
(`skipped` is `mut`: the core appends its own skips and sorts the list. The argument count trips clippy's `too_many_arguments` (limit 7): put `#[allow(clippy::too_many_arguments)] // the add's inputs are all distinct` on the function, the style of `eval/execute.rs`.)

**Steps:**
1. Make `Parsed`, `skip`, `added_by_of` and `MAX_SESSION_ID_BYTES` `pub(crate)` (fields of `Parsed` too). `Candidate`, `AddWrite`, `InsertFound`, `insert_found`, `parse_target`, `shape_error` stay private.
2. In `add_items`, keep everything up to and including the loop that builds `fitting` and `skipped` (the queue lookup, `added_by_of`, the 1–1 000 check, `parse_target`, `refuse_if_any`, `shape_error`). Move everything from `let ctx = open_query(...)` to the final `Ok((StatusCode::CREATED, error::Json(AddedQueueItems { … })))` into `add_found_targets`, **verbatim** (the three `match target` arms, the `indexes`/`found` mapping, `insert_found`, the `blocked` skips, the sort, the `tracing::info!` with the same message and fields, and the `AddedQueueItems` it builds). `add_items` then ends with
   ```rust
   let answer = add_found_targets(&state, org, project, queue_id, target, added_by, fitting, skipped).await?;
   Ok((StatusCode::CREATED, error::Json(answer)))
   ```
3. Update the doc comments: `add_items` keeps its text; the core's comment is the contract above. Do not change a message string, an order of checks or an error.

**Tests:** none new (a pure move); every existing test of the file (`add_items_by_id`, `add_items_checks_the_shape`, `add_span_and_session_items`, `add_items_skips_evaluation_traces`, `add_items_respects_the_guard`, `a_full_queue_is_422`, the `matching_*` tests) must still describe the same answers. Do not edit them.

**Done when:** `rustfmt --edition 2024 crates/server/src/routes/annotation_queues.rs` succeeds; `add_items` contains no `match target` over `Target::Trace`/`Span`/`Session` lookups; `grep -n "fn add_found_targets" crates/server/src/routes/annotation_queues.rs` finds one definition.

**Commit:** `Extract the annotation queue add core`

---

## M5.3a Request type and OpenAPI

**Goal:** the request type and the OpenAPI operation of `post_issue_queue_items` (D-IC-30), and the operation tables of `tests/openapi.rs`.

**Edit:** `crates/server/src/api/annotation_queues.rs`; `crates/server/src/api/openapi.rs`; `crates/server/tests/openapi.rs`.

**Contract:**
```rust
// api/annotation_queues.rs, next to AddQueueItems
/// Body of `POST /api/v1/projects/{project}/issues/{id}/queue-items` (D-IC-30).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueQueueItemsRequest {
    /// The queue: its id. A queue of this project.
    pub queue: Uuid,
    /// Most targets to add, 1 to 1 000. Default 100.
    #[serde(default, deserialize_with = "super::online_rules::present")]
    pub limit: Option<u32>,
}
```
`present` is `pub(super)` in `api/online_rules.rs` (as `api/datasets.rs` uses it), so `"limit": null` is a 400. The answer type is `AddedQueueItems` (existing, 201).

**Steps:**
1. Add the struct above. Keep the file's derive style (`AddQueueItems` also derives `Serialize`; this one needs only `Deserialize`, like `IssueDatasetItemsRequest`).
2. `api/openapi.rs`: add `IssueQueueItemsRequest` to the `use crate::api::annotation_queues::{…}` import; `set.insert::<IssueQueueItemsRequest>();` next to `set.insert::<AddQueueItems>();`; in the issues section, after `dataset_items`, add
   ```rust
   let queue_items = operation("post_issue_queue_items", "issues", "Send an issue's occurrences to an annotation queue")
       .description(Some("Project members and above, a read-write personal access token, or an API key with `write` (D-IC-30). Adds the targets of the most recent occurrences of the issue and of the issues merged into it (the issue and its 7 busiest merged issues over the last 14 days, newest first) to the annotation queue `queue`, up to `limit` targets (default 100, at most 1 000), through the same path as `POST …/annotation-queues/{id}/items`. A trace queue gets each occurrence's trace, a span queue its span, a session queue its session; each target once. An occurrence without that target is listed in `skipped`, its `index` being its position among the occurrences read newest first; so is a target that is unknown, hidden by the data window, an evaluation trace or blocked by a deletion. A target already in the queue counts as `existing`. The answer is 201 whatever the counts. 400 on `limit`, or on `issue` when the issue has no occurrence in the data window; 404 for a missing issue or queue; a queue that would pass 100 000 items is 422 and nothing is added."))
       .parameters(Some(vec![project_param(), id.clone()]))
       .request_body(Some(json_body("IssueQueueItemsRequest")))
       .responses(responses("201", json_of::<AddedQueueItems>("The counts and the skipped targets."), &[400, 401, 403, 404, 422, 429, 503]))
       .build();
   ```
   and the path `format!("{PROJECT}/issues/{{id}}/queue-items")` with `PathItem::new(HttpMethod::Post, queue_items)` right after the `dataset-items` path. (Check how the `dataset_items` path is added and copy its form exactly.) Do not regenerate `openapi.json`.
3. `crates/server/tests/openapi.rs`: in the `OPERATIONS` table, after the `post_issue_dataset_items` row, add `("post", "/api/v1/projects/{project}/issues/{id}/queue-items", "post_issue_queue_items"),` (same layout as its neighbours). In the if-chain of expected 4xx codes add a branch next to the `add_annotation_queue_items` one:
   ```rust
   } else if *id == "post_issue_queue_items" {
       // The test only collects 4xx codes; the operation also declares 503. No 413, 415: the body is small.
       expected_errors.extend(["400", "401", "403", "404", "422", "429"]);
   ```
   (read the chain first: if the helper that builds `responses(...)` adds codes other than those listed, the branch must equal exactly the 4xx codes the operation declares; `post_issue_dataset_items` is a model for where operations of the issues tag go.)

**Tests:** the table row and the error branch are the tests (`tests/openapi.rs` checks every route has its operation and the declared codes).

**Done when:** `rustfmt --edition 2024` on the three `.rs` files succeeds.

**Commit:** `Declare the issue queue-items operation`

---

## M5.3b The issue queue route

**Goal:** `POST /api/v1/projects/{project}/issues/{id}/queue-items` (D-IC-27 to D-IC-31).

**Edit:** new `crates/server/src/routes/issue_queue_items.rs`; `crates/server/src/routes/mod.rs` (`pub(crate) mod issue_queue_items;` next to `issue_dataset_items`); `crates/server/src/http/access.rs` (`routes.extend(crate::routes::issue_queue_items::routes());` after the `issue_dataset_items` line).

**Contract** (`routes/issue_queue_items.rs`):
```rust
/// Default and most targets one call adds (D-IC-28). The most is AQ's by-id cap.
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1_000; // == cetra_storage::MAX_TARGET_IDS, and the storage page cap
/// The span id metadata key of an occurrence (D-IC-18).
const SPAN_KEY: &str = "cetra.issue.span_id";

pub(crate) fn routes() -> Vec<RouteDef>   // one RouteDef, see step 1

/// The queue target of one occurrence, or the fixed reason it has none (D-IC-29, Q8).
enum Pick { Trace(TraceId), Span(TraceId, SpanId), Session(String) }
fn pick(kind: Target, score: &Score) -> Result<Pick, &'static str>

/// What a call adds: the targets (newest first, one per target, at most `limit`, each as a
/// `Parsed` whose `index` is the occurrence's position in `scored` after sorting) and the
/// occurrences skipped on the way.
struct Selection { targets: Vec<Parsed>, skipped: Vec<SkippedQueueItem>, scanned: usize }
fn select(kind: Target, scored: Vec<Score>, limit: usize) -> Selection
```
`Target` is `crate::meta::annotation_queues::Target`; `Parsed`, `skip`, `added_by_of`, `add_found_targets`, `MAX_SESSION_ID_BYTES` come from `crate::routes::annotation_queues` (M5.2).

**`pick`** (exact; `score.target`, `score.metadata`, `score.session_id` are the `cetra_core::score::Score` fields):
- `Target::Trace`: `ScoreTarget::Trace { trace_id }` and `ScoreTarget::Span { trace_id, .. }` → `Pick::Trace(trace_id)`; `ScoreTarget::Session` → `Err("the occurrence has no trace")`.
- `Target::Span`: `ScoreTarget::Span { trace_id, span_id }` → `Pick::Span`; `ScoreTarget::Trace { trace_id }` with `metadata.get(SPAN_KEY)` that parses as a `SpanId` → `Pick::Span(trace_id, span_id)`; anything else (no key, an unparsable value, a session target) → `Err("the occurrence has no span")`.
- `Target::Session`: `ScoreTarget::Session { session_id }` → that id; `ScoreTarget::Trace`/`Span` → `score.session_id`; the id must be 1 to `MAX_SESSION_ID_BYTES` bytes, else (absent, empty, too long) → `Err("the occurrence has no session")`.

**`select`**: sort `scored` by `created_at` descending, then `score_id` descending (a stable total order). Walk it with `enumerate`; for each occurrence `pick(kind, score)`: `Err(reason)` → `skipped.push(skip(index, reason))`; `Ok(pick)` → if its key was already seen (a `HashSet` of the pick's key, per kind: trace id, `(trace, span)` or session id) drop it silently; else push `Parsed { index, trace, span, session }` (only the fields of the queue's kind set, as AQ's `shape_error` requires) and stop the walk when `targets.len() == limit`. `scanned` is the number of occurrences visited. Pure: no I/O.

**Steps:**
1. `routes()`: `RouteDef { method: POST, path: "/api/v1/projects/{project}/issues/{id}/queue-items", access: Access::Project { perm: ProjectPerm::ManageQueues, session_only: false }, documented: true, handler: post(send_issue_to_queue) }` (no extra body layer: the body is `{queue, limit}`).
2. Handler `send_issue_to_queue(State(state), Extension(principal), Extension(access), error::Path(IssuePath { id }), error::Json(body): error::Json<IssueQueueItemsRequest>)` (`#[derive(Deserialize)] struct IssuePath { id: Uuid }`), in this order:
   1. `limit = body.limit.unwrap_or(100)`; outside `1..=1000` → `bad_request(vec![field_error("limit", "must be between 1 and 1000")])`. Imports and helper exactly as `issue_dataset_items.rs`: `use crate::eval::rule_check::field_error;` (not `crate::http::error::field_error`, which also exists and has another shape), `use crate::api::common::FieldError;` and a local `fn bad_request(errors: Vec<FieldError>) -> ApiError { ApiError::BadRequest { detail: None, errors } }`. Step 7's 400 uses it too.
   2. The issue: `issues_store::get_issue(state.meta.reader(), project, id)`; `None` → `ApiError::NotFound`. Any state is allowed.
   3. The queue: `meta::annotation_queues::get(state.meta.reader(), project, body.queue)`; `None` → `ApiError::NotFound` (a queue of another project is `None` too, `get` is scoped by project). Its `target` is the kind.
   4. `let added_by = added_by_of(&principal)?;`.
   5. The group, as M4.11 step 3 (the issues store is imported as `use crate::meta::issues as issues_store;`, because `store` would be ambiguous next to `meta::annotation_queues`; `get_issue` and `merged_ids` below are `issues_store::…`): `now = state.clock.now_ns()`, `window = TimeWindow::new(now.saturating_sub(OCCURRENCE_DEFAULT_PERIOD_NS), now)?`, `merged = issues_store::merged_ids(...)`, `names = names_of(id, &merged)`, `counts = tallies(&state, org, project, &names, window).await?`, `group = example_issues(id, &merged, &counts)` (all from `crate::issues::read`; the same code as `routes/issue_dataset_items.rs`, so the two routes pick the same occurrences: **if you find yourself copying more than these five lines, move them into a `pub(crate)` function of `issues/read.rs` used by both routes and tell the coordinator; do not edit `issue_dataset_items.rs` otherwise**).
   6. `ctx = open_query(&state, org, project, None).await?`; `pairs = occurrence_scores(&ctx, project, &group, limit as usize, Some(window)).await?` (`limit` rows per issue, Q4), which returns `Vec<(Uuid, Score)>`; `let scored: Vec<Score> = pairs.into_iter().map(|(_, score)| score).collect();`.
   7. `scored.is_empty()` → `ApiError::BadRequest` with the field error `issue`: "the issue has no occurrences in the data window".
   8. `selection = select(kind, scored, limit as usize)`.
   9. If `selection.targets` is empty, the answer is `201 {added: 0, existing: 0, skipped}` with `selection.skipped` (Q2); otherwise `add_found_targets(&state, org, project, body.queue, kind, added_by, selection.targets, selection.skipped).await?` and answer `(StatusCode::CREATED, error::Json(answer))`. (Do not call the core with no targets: it would run a pointless insert.)
   Log with ids and counts only, before `selection.targets` moves into the core: `tracing::info!(project_id = %project, issue_id = %id, queue_id = %body.queue, scanned = selection.scanned, targets = selection.targets.len(), skipped = selection.skipped.len(), "issue occurrences sent to a queue")` is the only added line (the core logs the add itself).
3. `routes/mod.rs` and `http/access.rs`: one line each.

**Tests** (`routes/issue_queue_items.rs`, `TestApp`). Build sources, issues and occurrence scores with `crate::test_support::issues::{put_source, put_issue, occurrence, source_row, issue_row}` (occurrences take `Some(span)` for the span metadata); commit traces with a local `commit_trace(app, seeded, trace_hex, span_hex, session: Option<&str>)` (clones `crate::test_support::dataset_span(0)`, sets `trace_id`, `span_id`, `parent_span_id = None`, `session_id`, `start_time` near `app.state.clock.now_ns()`, commits with `app.commit_spans(&seeded.project, vec![span])`, as `commit_span` of the AQ tests does). `crate::test_support::issues::occurrence` always writes `session_id: None`, so the module has its own `occurrence_in(app, seeded, source, issue, trace, at, session: Option<&str>, span: Option<&str>)`: a copy of that helper (`test_support/issues.rs`, the `occurrence` function) that writes through `crate::scores::{self as score_writes, Writer}` with `Writer::Issue { source_id }` and `score_writes::occurrence_id(project, trace_id, issue, at)` and sets `session_id: session.map(str::to_owned)`; the shared helper is not changed. The session-queue test and the deletion-guard session case use it. A queue needs a score definition. Copy these from the private test module of `routes/annotation_queues.rs` (re-read it; they are 5 to 25 lines each): `base(seeded)` (the queues path), `send(app, method, path, cookie, body)`, `define(app, project, name, archived)` (`insert_definition` through `app.state.meta.write`; imports `DefinitionFields`, `insert_definition`, `ScoreDirection`, `ScoreDataType`), `create_of(app, seeded, name, target, definition)` (`POST …/annotation-queues`, asserts 201) and `trace_id(n)`. List items with `GET …/annotation-queues/{id}/items` as AQ's `items_of` does. Helpers `send(app, seeded, issue, body)` and `ok(...)` as in `issue_dataset_items.rs`. Ids: `trace(n) = format!("a1{n:030x}")`, `span(n) = format!("b1{n:014x}")`.

Pure tests of `pick` and `select` (build `Score` values with a local `score(target, session, metadata, created_at, score_id)` helper that fills the other fields of `cetra_core::score::Score` with fixed values):
- `pick_by_queue_kind`: the table of `pick` above, every arm: trace/span/session occurrence for each kind, the span key present, absent and unparsable, a session id empty and of 256 bytes.
- `select_takes_the_newest_per_target_and_stops_at_the_limit`: 5 distinct traces and `limit 2` give the 2 newest, `scanned == 2`; a duplicate trace (two occurrences of one trace) gives one target and is not in `skipped`.
- `select_lists_occurrences_without_the_target_by_position`: span kind, occurrences `[span, none, span, none]` newest first give 2 targets and `skipped` indexes `[1, 3]` with "the occurrence has no span"; the walk stops at the limit, so an occurrence past it is neither a target nor skipped.
- `select_default_limit_is_the_callers`: 101 distinct traces with `limit 100` gives 100.
- `select_orders_ties_by_score_id`: two occurrences with the same `created_at` give a stable order.

Route tests:
- `a_trace_queue_gets_one_item_per_trace`: 3 occurrences on 2 traces (two issues of one group on one trace), both traces committed: `added == 2`, items' `trace_id` set equals the two traces, `span_id` and `session_id` are `None`, `added_by == User`.
- `limit_caps_the_targets_newest_first`: 5 traces with distinct occurrence times, `limit 2` adds the 2 newest; `limit 1000` is accepted; `limit 0`, `limit 1001` and `"limit": null` are 400 on `limit`.
- `a_span_queue_gets_the_occurrence_spans`: one occurrence with its (committed) span, one without a span key: `added == 1`, the item has that `span_id`, `skipped == [{index: 1, message: "the occurrence has no span"}]` (the index of the occurrence read newest first; build the times so it is deterministic); two occurrences of one span from two issues of the group add one item.
- `a_session_queue_gets_the_distinct_sessions` (uses `occurrence_in`): three occurrences on traces in two sessions plus one trace without a session: `added == 2`, items' `session_id` set equals the two sessions, one skip "the occurrence has no session"; `limit 1` adds the newest session only.
- `merged_issues_count_for_their_target` and `the_group_is_the_issue_and_the_seven_busiest_merged_issues` (as in M4.11, with trace queues): an occurrence of a merged issue is sent when its target is requested; of 9 merged issues with differing counts, the 2 least busy are absent; an occurrence older than 14 days is not read.
- `any_issue_state_is_allowed` (resolved, ignored).
- `existing_targets_are_counted`: sending twice gives `added == 0`, `existing == 2` the second time.
- `unknown_targets_are_skipped_by_the_core`: an occurrence whose trace was never committed comes back as `{index, "the trace was not found"}`; the others are added.
- `the_deletion_guard_blocks_a_target`: `app.state.deletions.guard.add_traces(project, &[trace], Uuid::now_v7(), now)` on one occurrence's trace gives `"a deletion blocks this target"` for it and the others are added; with a session queue, a session that holds the guarded trace is blocked.
- `a_full_queue_is_422`: fill the queue with 100 000 pending items in one statement through `app.state.meta.write` (`WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000) INSERT INTO annotation_queue_items (id, queue_id, project_id, trace_id, target_time, added_by_kind, added_by_id, created_at, updated_at) SELECT printf('01900000-0000-7000-8000-%012x', i), <queue id>, <project id>, printf('%032x', i), 1000, 'user', <the seeded user id>, 1000, 1000 FROM n`, binding the three ids as text; `status` is left to its default `pending`; the trace ids cannot collide with the new trace's because the test's trace is `a1…`, above `printf('%032x', 100000)`; the unique index is on `(queue_id, trace_id, span_id, session_id)`), then send one new trace: 422 with code `too_many_items`, and the queue still has exactly 100 000 items. If this test takes more than a few seconds, stop and report instead of lowering the cap.
- `an_issue_without_occurrences_is_400`: field `issue`, "the issue has no occurrences in the data window".
- `occurrences_that_fit_no_target_answer_201_with_all_skipped` (Q2): span queue, occurrences without span keys: 201, `added == 0`, every occurrence in `skipped`.
- `a_missing_issue_is_404`, `an_issue_of_another_project_is_404`, `a_missing_queue_is_404`, `a_queue_of_another_project_is_404` (`app.add_project(&seeded, "playground", None)` of `test_support/mod.rs` returns `(ProjectRow, key)`; as in `an_issue_of_another_project_is_404` of `issue_dataset_items.rs`, put the issue in that project with `put_source`/`put_issue` for `other.id`; for the queue, `define` a definition in `other.id` and `POST` to `/api/v1/projects/{other.id}/annotation-queues` with the owner's cookie, then call the route of the seeded project with that queue's id), `a_queue_that_is_not_a_uuid_is_400`.
- `permissions`: a viewer session is 403; a member session is 201 (`added_by == User`); a read-only API key is 403; a `write` key (`app.write_key(&seeded)`) is 201 and its items have `added_by == Key`; a read-only personal access token (`app.personal_token(member.id, false)`) is 403.

**Done when:** `rustfmt --edition 2024` on the three `.rs` files succeeds; the file contains no call to `parse_target`, `shape_error`, `insert_found` or `store::insert_items_tx` (AQ's checks are reached only through `add_found_targets`).

**Commit:** `Send an issue's occurrences to an annotation queue`

---

## M5.4 Web: shared queue dialog and the issue dialog

**Goal:** the add dialog of AQ becomes a shell that both the trace/span/session pages and the issue page use, and the issue page's dialog exists (D-IC-27).

**Edit:** `web/src/components/annotation-queues/add-to-queue-dialog.tsx` (its test file is read, not edited).

**API contract:** the operation `post_issue_queue_items`: `POST /api/v1/projects/{project}/issues/{id}/queue-items`, body `{ queue: string; limit?: number }`, `201` `AddedQueueItems` (the type `…/annotation-queues/{id}/items` answers).

**Steps:**
1. Extract the shell. Add and export
   ```tsx
   export interface QueueTargetDialogProps {
     open: boolean;
     onOpenChange: (open: boolean) => void;
     title: string;
     description: string;
     /** The kinds of queue the picker lists. One kind: options show the name only; several: `name · kind`. */
     kinds: readonly QueueKind[];
     /** The kind "New queue…" starts with. */
     defaultKind: QueueKind;
     /** Adds to the queue `queueId`; resolves with the server's answer. */
     add: (queueId: string) => Promise<Schema<"AddedQueueItems">>;
     /** True while that request runs (the caller's mutation `isPending`). */
     adding: boolean;
     /** What a `skipped` row's `index` names in the result list. */
     skippedLabel: (index: number) => string;
     /** Called once the items are added. */
     onAdded?: () => void;
   }
   export function QueueTargetDialog(props: QueueTargetDialogProps): JSX.Element
   ```
   `QueueTargetDialog` is today's `AddToQueueDialog` shell (the `Dialog`, the busy guards on outside click and Escape, header) with the title and description from props, rendering `AddToQueueForm` with `{ kinds, defaultKind, add, adding, skippedLabel, onAdded, onClose }` in place of `{ target, items, add, onAdded, onClose }`.
2. In `AddToQueueForm` (rename it `QueueTargetForm`): `listed = queues.filter((queue) => kinds.includes(queue.target))`; the option label is `queue.name` when `kinds.length === 1`, else `` `${queue.name} · ${queue.target}` ``; `QueueDialog`'s `defaultTarget` is `defaultKind`; `onSubmit` calls `await add(picked.id)` where it called `add.mutateAsync(...)`, then the same invalidations (`annotationQueueWriteKeys`), `onAdded?.()` and `setResult`; the submit button's `pending` is `adding`; the result view renders `skippedLabel(row.index)` where it rendered `itemLabel(items[row.index])`; errors keep `queueFullMessage(caught) ?? problemMessage(caught, "The items could not be added.")` (the 422 text applies to the issue route too; a 400 on `issue` shows the server's message through `problemMessage`, which joins field errors).
3. `AddToQueueDialog` keeps its exact props and behaviour: it calls `useAddItems()` and renders `QueueTargetDialog` with the title it has today, the same description, `kinds={[target]}`, `defaultKind={target}`, `adding={addItems.isPending}`, `add={(id) => addItems.mutateAsync({ params: { path: { project: ref, id } }, body: { items: [...items] } })}` (`ref = projectRef(org, project)` from `useProject()`), `skippedLabel={(index) => itemLabel(items[index])}` and its `onAdded`. `AddToQueueButton`, `addToQueueReason`, `queueFullMessage`, `useProjectQueues`, `ADD_ITEMS_MAX` and the types stay exported unchanged.
4. Add and export, in the same file:
   ```tsx
   export interface SendIssueToQueueDialogProps {
     open: boolean;
     onOpenChange: (open: boolean) => void;
     /** The issue whose newest occurrences are sent. */
     issueId: string;
   }
   /** Sends the newest occurrences of an issue to a queue of any kind (D-IC-27). */
   export function SendIssueToQueueDialog(props: SendIssueToQueueDialogProps): JSX.Element
   ```
   It calls `$api.useMutation("post", "/api/v1/projects/{project}/issues/{id}/queue-items")` and renders `QueueTargetDialog` with title `Send to a queue`, description `Adds the issue's newest occurrences (up to 100) to a queue. A trace queue gets their traces, a span queue the spans, a session queue the sessions, each once. An item already in the queue is not added twice.`, `kinds={["trace", "span", "session"]}`, `defaultKind="trace"`, `adding` its `isPending`, `add={(queue) => send.mutateAsync({ params: { path: { project: projectRef(org, project), id: issueId } }, body: { queue } })}` and `skippedLabel={(index) => `Occurrence ${index + 1}`}` (the index is the position among the occurrences, D-IC-29). No limit is sent (Q5).

**Tests:** none added here. `add-to-queue-dialog.test.tsx` has no render helper for the dialog: its tests go through `renderApp(url)` on the trace, span and session pages, and the issue dialog's only host is the issue page. Its existing tests (trace, span and session pages, new queue, full queue, skipped list, viewer) must pass unchanged and are the proof that the extraction changed nothing. The issue dialog's tests are in M5.5, through the issue page.

**Done when:** `pnpm --dir web exec biome format --write` on the edited file succeeds; `add-to-queue-dialog.test.tsx` is not edited; `AddToQueueDialog`'s props and `AddToQueueButton` are unchanged (their callers are not edited).

**Commit:** `Add the send-to-queue dialog for issues`

---

## M5.5 Web: issue page button

**Goal:** **Send to queue** on the issue page (D-IC-27).

**Edit:** `web/src/components/issues/issue-actions.tsx`; `web/src/routes/$org/$project/issues/$issueId.tsx`; `web/src/routes/$org/$project/issues/$issueId.test.tsx`; `web/src/mocks/writes.ts`; `web/src/mocks/writes.test.ts` (only if it hard-codes operation ids: check and edit only then).

**Steps:**
1. `issue-actions.tsx`: `IssueActions` gets a prop `canSendToQueue: boolean`. For a non-merged issue, after "Add examples to dataset": when `canSendToQueue`, a `Button type="button" variant="outline"` **Send to queue** opening `SendIssueToQueueDialog` (state `queueOpen`, rendered like `AddIssueExamplesDialog`, with `issueId={issue.id}`). Import it from `@/components/annotation-queues/add-to-queue-dialog`. The component still returns `null` for merged issues. Update the doc comment's button list.
2. `$issueId.tsx`: pass `canSendToQueue={canManageQueues(data.role)}` (import `canManageQueues` from `@/lib/roles`).
3. `mocks/writes.ts`: add to `WRITE_OPERATIONS`, after `post_issue_dataset_items`, `{ operationId: "post_issue_queue_items", method: "post", path: "/api/v1/projects/{project}/issues/{id}/queue-items" }`, and the handler `post_issue_queue_items: () => HttpResponse.json(annotationQueueItemsAddedDocument, { status: 201 })` with `import annotationQueueItemsAddedDocument from "../../../fixtures/api/static/annotation-queue-items-added.json";` placed by hand in sorted position, right after the `annotation-queue-item.json` import and before `api-key-created.json` (Biome's formatter does not sort imports, but `biome check` flags an unsorted block; the existing block orders `annotation-queue`, `annotation-queue-item`, `api-key-created`). The fixture exists; check the file does not already import it under another name. `writes.test.ts` derives its list from the OpenAPI document, so it needs no edit unless it hard-codes ids.

**Tests** (`$issueId.test.tsx`, in the file's style: `serveIssue()`, `renderApp(PAGE)`, click **Send to queue**; the queue list is served with `server.use` on `GET /api/v1/projects/:project/annotation-queues`, as `serveQueues` does in `add-to-queue-dialog.test.tsx`, holding a trace, a span and a session queue). The two button tests extend the file's existing role and merged-issue tests (same setup and role switches), they do not duplicate them; the dialog tests are new:
- `shows Send to queue to members and not to viewers`: a member sees the button, a viewer (role `viewer`, as the file's existing viewer test sets it) does not; the annotator role, if the file's role helper has one, follows `canManageQueues` (member and above).
- `hides Send to queue on a merged issue`: no button, no other action.
- `lists every queue with its kind`: after the click the picker shows `Relevance review · trace`, a span queue and a session queue.
- `sends the issue to the chosen queue`: an MSW handler on `/api/v1/projects/:project/issues/:id/queue-items` captures the issue id and the body (`{ queue: "<id>" }`, no `limit`) and answers `fixtures/api/static/annotation-queue-items-added.json`; the result says `Added 2 to <name>.` and has the **Open queue** link.
- `names the skipped occurrences`: the fixture's skipped row (index 2) renders `Occurrence 3` with the server's message.
- `shows the 400 message when the issue has no occurrences`: the handler answers a problem with the field error `issue`; the form shows "the issue has no occurrences in the data window".
- `explains a full queue`: a 422 problem with code `too_many_items` shows "The queue is full (100 000 items)."
- `creates a new queue first`: choosing "New queue…" opens the create form, the created queue is picked and the send goes to its id.
- `Create alert`, `Add examples to dataset` and the existing tests stay as they are.

**Done when:** `pnpm --dir web exec biome format --write` on the edited files succeeds; `IssueActions`' other props are unchanged.

**Commit:** `Add send to queue to the issue page`

---

## M5.6 Docs

**Goal:** specs, site pages, changelog and roadmap describe M5.

**Edit:**
- `docs/spec/06-evaluation.md` (§ Issues, the **Links** paragraph);
- `docs/spec/09-api.md` (§ Issues: the route row; § Annotation queues: one cross-reference sentence);
- `docs/spec/11-ui.md` (the issue page paragraph);
- `website/src/content/docs/docs/evaluation/issues.mdx`;
- `website/src/content/docs/docs/evaluation/annotation-queues.mdx`;
- `CHANGELOG.md`;
- `docs/roadmap.md`.

**Steps:**
1. Spec 06: in the **Links** paragraph add: "An issue can send its newest occurrences to an annotation queue of any kind (09 § Issues, D-IC-27 to D-IC-31)." and replace the last sentence "These links read issues; none of them changes an issue's state, title or merges." by exactly: "These links read issues; the dataset and queue links write datasets and queue items; none of them changes an issue's state, title or merges."
2. Spec 09 § Issues, after the `dataset-items` row, add the row:
   `| POST /projects/{project}/issues/{id}/queue-items | {queue, limit?}, queue an annotation queue id, limit 1 to 1 000 (default 100) → 201 with {added, existing, skipped} as POST /annotation-queues/{id}/items answers. Adds the targets of the newest occurrences of the issue and of the seven merged issues with the most occurrences in the last 14 days, in any issue state: a trace queue gets each occurrence's trace, a span queue its span (the cetra.issue.span_id of the occurrence; error issues and span-level candidates have it), a session queue its session, each target once, at most limit targets. An occurrence without that target is skipped ("the occurrence has no trace", "…no span", "…no session"; index = its position among the occurrences read newest first). Everything else is AQ's add: the data window, evaluation traces, the deletion guard re-checked in the insert transaction, one item per target, past 100 000 items 422 too_many_items with nothing added (D-AQ-8). Permission ManageQueues: members and above with a session, a read-write personal access token or an API key with write. 400 on limit and on issue when the issue has no occurrence in the data window; 404 for a missing issue or queue (of another project too); the answer is 201 whatever the counts, also when every occurrence is skipped (D-IC-30) |` (write it in the table's own style, with backticks as its neighbours use).
   In § Annotation queues, after the `POST /annotation-queues/{id}/items` row's text, add: "An issue's occurrences are added through `POST /issues/{id}/queue-items` (§ Issues)." as a sentence below the table or in that row, wherever the section puts notes.
3. Spec 11: in the **Issue page** paragraph add **Send to queue** (for members and above, `ManageQueues`) after **Add examples to dataset**: "**Send to queue** opens a dialog with the project's queues, each with its kind (or **New queue…**); it adds up to 100 of the issue's newest occurrences as traces, spans or sessions by the queue's kind, and shows the result with the occurrences that were skipped (D-IC-27 to D-IC-31). Like the other actions it is hidden on a merged issue." Also in that paragraph, replace the dataset dialog's "a count of up to 100 traces" by "(it adds up to 100 traces)", since the dialog has no count field.
4. `issues.mdx`: after "Add examples to a dataset" add `## Send to a queue` (plain prose and a numbered list, per `website/STYLE.md`, no code block): 1. Open the issue and select **Send to queue**. You need permission to manage annotation queues (members and above). 2. Choose a queue, or select **New queue…**. 3. Select **Add**. Then: cetra takes the newest occurrences of the issue and of the issues merged into it, up to 100. A trace queue gets one item per trace. A span queue gets the span of each occurrence, which error issues and span-level issues have; an occurrence without a span is listed as not added. A session queue gets one item per session; an occurrence without a session is listed as not added. An item that is already in the queue is not added twice, and a trace or session that a deletion is removing is skipped. The result says how many items were added, how many were already there and which were skipped. Through the API you can send up to 1,000, see the API reference. Link to `/docs/evaluation/annotation-queues/`. Also fix the sentence of "Add examples to a dataset", step 3 ("Set the number of traces…"): the dialog has no count field (Q5): replace steps 3 and 4 by "3. Select **Add**." and say "cetra adds up to 100 traces; the API takes a `limit` of 1 to 100." (check the API reference page anchor before linking).
5. `annotation-queues.mdx` § Add items: add a bullet to the list of places ("On an issue page, **Send to queue** adds the issue's newest occurrences. See [Issues](/docs/evaluation/issues/#send-to-a-queue).").
6. `CHANGELOG.md` § `[Unreleased]` § Added, after the line of "Add examples to dataset": `- Issues: "Send to queue" adds an issue's newest occurrences (traces, spans or sessions, by the queue's kind) to an annotation queue (D-IC-27 to D-IC-31).` No upgrade note (no migration).
7. `docs/roadmap.md` IC row: replace "M5 to do, waiting for the user." by "M5 implemented (\"Send to queue\" from an issue, through the shared AQ add core; D-IC-27 to D-IC-31)" and `D-IC-n` stays in the decisions cell; list the PR when known.

**Tests:** none (the Closing runs `just check-docs`).

**Done when:** by hand, `grep -rn "waits for track AQ\|M5 to do" docs/spec docs/roadmap.md docs/mvp-decisions.md website/src/content` finds nothing; every new site link exists; no code block was added to the site pages.

**Commit:** `Document the IC M5 queue link`

---

## Closing (once, after every phase is committed)

Run in this order, from the worktree, one command at a time (the machine is loaded; do not run them in parallel):
1. `just fmt`.
2. `just openapi` (regenerates `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `web/src/mocks/generated/*`, `website/src/generated/openapi.json`). Check by hand that the diff shows exactly: the new `post_issue_queue_items` operation (tag `issues`) with `IssueQueueItemsRequest` and `AddedQueueItems`, and nothing else.
3. `just api-examples` (expected: no change: the request type has no example file, as `IssueDatasetItemsRequest` has none, and the answer reuses `annotation-queue-items-added.json`). If `crates/server/tests/api_examples.rs` fails on a missing request example, add `requests/issue-queue-items.json` (`{"queue": "01900000-0000-7000-8000-000000000a01"}`) with the matching `roundtrip::<IssueQueueItemsRequest>` line and say so in the report.
4. `just docs-gen` (expected: no change; M5 adds no CLI flag or SQL view).
5. `just check` (Rust, web, SDKs, docs). Fix failures with Haiku briefs under the same rules. Insta snapshots should not change (no migration, no schema change); if one does, find out why before accepting it.
6. Commit the generated files in one commit `Regenerate the IC M5 generated files`, then `just assert-clean`.
7. A Sonnet code review of the whole M5 diff against this plan (`git diff <main>..HEAD`), the review looking in particular for any copy of AQ's checks outside `add_found_targets`; fixes by Haiku with exact briefs.

Report (the coordinator relays it): what was done; any deviation from this plan, the design or the spec, and why; the answers given to Q1 to Q8; new dependencies (none); anything left unverified.

---

## Appendix A: decisions D-IC-27 to D-IC-31 (text to copy)

**For `00-design.md`** (sections after D-IC-25):

### D-IC-27 Send to queue
- The issue page gets **Send to queue**, next to "Add examples to dataset" and "Create alert", for callers who can manage queues (`ProjectPerm::ManageQueues`: members and above). It is hidden on merged issues, like the other actions. There is no bulk action on the issue list. The user chose this scope.
- Web: a dialog with the project's queues (each with its kind, and "New queue…") and the add result with the skipped list, the shell shared with AQ's add dialog.

### D-IC-28 Which occurrences
- The same selection as D-IC-22: the issue and its 7 most-occurring merged issues (`example_issues`), over the default 14-day occurrences window ending now, any issue state, newest occurrence first. Default `limit` 100, `limit` 1 to 1 000 (AQ's by-id cap, D-AQ-8). `limit` counts targets, not occurrences.

### D-IC-29 Targets by queue kind
- A **trace** queue gets the occurrences' traces (one item per trace). A **span** queue gets each occurrence's span: `cetra.issue.span_id` in the occurrence score's metadata, or the span of a span-target occurrence (error issues and span-target LLM candidates have one). A **session** queue gets the distinct sessions of the occurrences (the score's `session_id`, or the session of a session-target occurrence). Each target is added once.
- An occurrence that has no such target (a trace queue with a session-target occurrence, a span queue without a span, a session queue without a session) is listed in `skipped` with a fixed message: "the occurrence has no trace", "the occurrence has no span", "the occurrence has no session". `skipped[].index` is the occurrence's position among the occurrences read newest first. A group whose occurrences all lack the target answers 201 with `added` 0; a group with no occurrence at all is a 400 on `issue`: "the issue has no occurrences in the data window".

### D-IC-30 The route
- `POST /api/v1/projects/{project}/issues/{id}/queue-items`, body `{"queue": "<queue id>", "limit": 1..=1000 (default 100)}`, permission `ManageQueues` (a session, a read-write personal access token, or an API key with `write`, as AQ allows). `queue` is an **id**, as every AQ route takes one (queue names are editable and AQ has no lookup by name). Answer: AQ's `AddedQueueItems` (`added`, `existing`, `skipped`) with 201, so the web reuses AQ's result view. 404 for an unknown issue or a queue not of the project; 400 on `limit`; 422 `too_many_items` from AQ's cap.

### D-IC-31 One add path, no migration
- The route goes through AQ's own add path: the core of `add_items` (lookup of the targets over the data window, evaluation traces left out, the deletion guard re-checked in the insert transaction, the 100 000-item cap, one item per target, `added_by`) is extracted into one `pub(crate)` function that both routes call, the way D-IC-22 extracted J's bulk-add core. No check is written twice, and AQ's API, limits and permissions are unchanged.
- Queue items have no metadata column, so an item does not record its issue (unlike the dataset item's `cetra.issue_id`). M5 adds no migration.

**For `docs/mvp-decisions.md`** (rows after `D-IC-26`):

| D-IC-27 | Send to queue | **The issue page gets "Send to queue" (`ManageQueues`), hidden on merged issues; no bulk action on the list (user).** |
| D-IC-28 | Which occurrences | **The same selection as "Add examples to dataset" (D-IC-22): the issue and its 7 busiest merged issues over the last 14 days, newest first; default 100, `limit` 1–1 000 (AQ's by-id cap).** `limit` counts targets. |
| D-IC-29 | Targets by queue kind | **A trace queue gets the occurrence traces, a span queue the occurrence spans (`cetra.issue.span_id`), a session queue the distinct sessions; each target once.** An occurrence without the target is listed in `skipped` with a fixed message. No occurrence at all is a 400 on `issue`; occurrences that fit no target are a 201 with `added` 0. |
| D-IC-30 | Send to queue route | **`POST …/issues/{id}/queue-items` (`{queue, limit}`, `ManageQueues`) answers AQ's `AddedQueueItems` with 201.** `queue` is an id, as in every AQ route. 404 for an unknown issue or queue; 422 `too_many_items` from AQ's cap. |
| D-IC-31 | One add path, no migration | **The route goes through the core extracted from AQ's `add_items` (data window, deletion guard re-checked in the insert transaction, 100 000-item cap, one item per target, `added_by`); no check is duplicated.** Queue items carry no issue reference, and M5 has no migration. |
