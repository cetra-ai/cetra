# AQ M3 — Adds by filter

**Summary.** "Add matching to queue…" for the traces, spans and sessions lists (D-AQ-8). Two routes:
- `POST …/annotation-queues/{id}/items/matching/preview` counts what the queue kind's list filter matches.
- `POST …/items/matching` adds up to 10 000 of them.

They rest on storage scans that return each target with its `target_time`:
- the trace scan is `filter_trace_ids`, changed to return starts;
- the span and session scans are new.

**Prerequisites.** M2 is committed: Q1's `reads/targets.rs` (`TraceTarget`, `SpanTarget`, `SessionTarget`, `trace_targets`, `span_targets`, `session_targets`) and Q4b's `add_items` exist. Branch `claude/aq-impl` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/aq-annotation-queues-f38be7`.

**Read first.**
- `00-design.md` § Adding items, and D-AQ-8.
- `crates/storage/src/reads/deletions.rs`: `FilterDeletionRequest` and `filter_trace_ids` (lines 33–115), and its tests.
- `crates/storage/src/reads/traces.rs`: `SpanListRequest` and `list_frame` (lines 921–996), and `drop_adjacent_copies`.
- `crates/storage/src/reads/sessions.rs`: `SessionListRequest`, `list` (line 271) and `session_rows` (around line 880).
- `crates/server/src/routes/deletions.rs`: `preview_deletion` (line 392) and `matching` (line 447).
- `crates/server/src/api/deletions.rs`: `DeletionPreviewRequest` (line 184).
- `crates/server/src/routes/read.rs`: `check_search` (line 56), `open_query` and `READ_TIMEOUT`.
- `crates/server/src/routes/annotation_queues.rs` (M2).

## What it is, and what it is not

**What works when it's done:**
- A trace, span or session queue can count, then add, everything its kind's list shows for a filter, a search (traces and spans only), a `[from, to)` range and the experiments toggle: at most 10 000 per call, never evaluation traces, within the data window.
- Deletion-guarded targets are counted as blocked and not added.

**Out of scope:**
- the UI dialog (M4);
- SDK wrappers (M5 does not wrap these routes).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AQ-8 | Preview then add, at most 10 000, unspecified which when capped, never eval, guard re-checked in the insert transaction; scans return `target_time` |
| D-AQ-9 | `target_time` of a session = its earliest visible trace start |
| D-AQ-11 | `ManageQueues` |
| D-M-5 | The "Delete matching" precedent: count first, cap + 1 |

Plan-level choices:
- **Answers.** The add answers `{added, existing, blocked, capped}`. `blocked` counts guarded targets; the design's table lists the other three.
- **Session target time.** Session targets take `start_time` and `trace_ids` from M2's `session_targets` (no list window, eval excluded, experiments included), so a session added by filter and one added by id get the same `target_time` and the same guard check.
- **Experiment traces.** The span and trace scans honour `include_experiments` from the body, as the lists do. The by-id add always includes experiment traces.
- **The `filter_trace_ids` answer.** `filter_trace_ids` changes its answer to `Vec<TraceTarget>`. There is no sibling function (AGENTS.md: no compatibility layers). Its callers take `.trace_id`.

## Versions

No new dependency.

## Phases

```
R1 (storage scans) ─► R2 (routes)
```

Both are Rust and run in order.

---

### R1 — Storage scans with target times

**Goal.** `filter_trace_ids` returns trace starts, and new `filter_span_targets` and `filter_session_targets` apply the spans and sessions lists' filter semantics up to a limit.

**Edit:**
- `crates/storage/src/reads/deletions.rs`
- `crates/storage/src/reads/traces.rs` (extract the span-list conjuncts only)
- `crates/storage/src/reads/sessions.rs`
- `crates/storage/src/reads/targets.rs`
- `crates/storage/src/reads/mod.rs` and `crates/storage/src/lib.rs` (exports)
- `crates/storage/src/model_tests/observe.rs` and `crates/storage/src/model_tests/check.rs` (only the `filter_trace_ids` caller and its comparison)
- `crates/server/src/deletion/resolve.rs` and `crates/server/src/routes/deletions.rs` (only take `.trace_id` from the new answer)

**Steps.**
1. **Traces.** In `reads/deletions.rs`, change `filter_trace_ids` to return `Result<Vec<TraceTarget>, RunError>`:
   - select `trace_id` and `start_time` instead of `trace_id` alone;
   - keep one row per trace id (a `BTreeMap<TraceId, i64>`, keeping the smallest start if a trace appears twice);
   - answer sorted by trace id;
   - read `start_time` as Q1's `trace_targets` does;
   - leave the semantics and the limit unchanged;
   - update the doc comment.
2. **Trace callers.** Update the callers to map `.trace_id`:
   - `deletion/resolve.rs` (`filter_targets`, around line 131);
   - `routes/deletions.rs` (`matching`, around line 447; only the count is used there);
   - `model_tests/observe.rs` (`observe_filter_trace_ids`, around line 505: map with `.iter().map(|t| hex(t.trace_id.as_bytes()))`, so the observer's output is unchanged and `check.rs` needs no change);
   - the unit tests in `reads/deletions.rs`.
3. **Span frame.** In `reads/traces.rs`, extract the conjunct building of `list_frame` (time window on the span's start, `exclusions(ctx, FilterView::Spans, …)` with its `references_environment` flag taken from the parsed filter, and `search(FilterView::Spans, …)`, then `filter.apply`) into `pub(crate) async fn span_scope_frame(ctx: &QueryContext, filter: Option<Filter>, search: Option<&str>, window: TimeWindow, include_eval: bool, include_experiments: bool) -> Result<DataFrame, RunError>` (the parsed `Filter` type `list_frame` already uses, passed the way `list_frame` passes it). The cursor, sort, limit, `ListPlan` and the `strict` branch stay in `list_frame`, untouched. `list_frame` calls the new function, so the span list's behaviour is unchanged and its tests pass untouched.
4. **Spans.** In `reads/targets.rs`, add:
   ```rust
   /// Filter text as the list routes take it; parsed inside storage (`Filter` is crate-private).
   pub struct SpanTargetFilter<'a> {
       pub filter: Option<&'a str>,
       pub search: Option<&'a str>,
       pub window: TimeWindow,
       pub include_experiments: bool,
       pub limit: usize,
       pub timeout: Duration,
   }
   /// The sessions list has no search, so this request has none.
   pub struct SessionTargetFilter<'a> {
       pub filter: Option<&'a str>,
       pub window: TimeWindow,
       pub include_experiments: bool,
       pub limit: usize,
       pub timeout: Duration,
   }
   /// Targets found, plus whether more than `limit` matched before any target was dropped.
   pub struct FilteredTargets<T> { pub targets: Vec<T>, pub capped: bool }
   pub async fn filter_span_targets(ctx: &QueryContext, request: SpanTargetFilter<'_>) -> Result<FilteredTargets<SpanTarget>, RunError>;
   pub async fn filter_session_targets(ctx: &QueryContext, request: SessionTargetFilter<'_>) -> Result<FilteredTargets<SessionTarget>, RunError>;
   ```
   `TimeWindow` is the type `FilterDeletionRequest` uses. Each function trims the filter and parses it inside storage the way `filter_trace_ids` does (`present()`, then `ctx.parse_filter(FilterView::Spans, text)` or `FilterView::Sessions`); a bad filter is the `RunError` `filter_trace_ids` returns for it, which the server already maps to `400 invalid_filter`. `limit` is the cap (the route passes 10 000).

   `filter_span_targets` follows these steps:
   1. Start from `span_scope_frame(ctx, parsed, search, window, false, include_experiments).await`.
   2. Select `trace_id` and `span_id`, and stream the batches.
   3. Collect distinct pairs in a `BTreeSet` until it holds `limit + 1` pairs, then stop. Copies of a span share their ids, so the set dedups them, as `drop_adjacent_copies` does for the list. `capped` = the set reached `limit + 1`; then keep only the first `limit` pairs.
   4. Look the trace starts up with `trace_targets` in chunks of `MAX_TARGET_IDS` trace ids, and pair them. `target_time` is the trace's start, even when the trace starts before the window's `from` (the window applies to the span's own start).
   5. A pair whose trace is not found (hidden meanwhile) is dropped; `capped` keeps its value from step 3.
   6. Answer sorted by `(trace_id, span_id)`.
   7. The whole call runs under `timeout`.
5. **Sessions.** In `reads/sessions.rs`, add the following. It is `session_rows` with the list's window, the eval exclusion and the filter, selecting `session_id`, and stopping at `limit` distinct sessions.
   ```rust
   pub(crate) async fn filtered_session_ids(ctx: &QueryContext, filter: Option<Filter>, window: TimeWindow, include_experiments: bool, limit: usize) -> Result<Vec<String>, RunError>
   ```
   (`Filter` by value, as `session_rows` takes it.) It returns at most `limit` distinct ids.

   In `targets.rs`, `filter_session_targets` follows these steps:
   1. Parse the filter (see above) and call `filtered_session_ids` with `limit + 1`. `capped` = it returned `limit + 1` ids; keep the first `limit`.
   2. Fetch `session_targets` in chunks of `MAX_TARGET_IDS`, so `start_time` and `trace_ids` come from the no-window lookup.
   3. A session the scan found but `session_targets` does not answer (for example, one whose only traces are evaluation traces) is dropped silently.
   4. Answer sorted by session id.
   5. Run under `timeout`.
6. Export `SpanTargetFilter`, `SessionTargetFilter`, `FilteredTargets`, `filter_span_targets` and `filter_session_targets` through `reads/mod.rs` and `lib.rs`.
7. `filter_trace_ids` reads `start_time` with a new private helper next to `collect_ids` (`collect_ids` reads column 0 only): `fn collect_targets(batches: &[RecordBatch]) -> Result<BTreeMap<TraceId, i64>, RunError>`, reading column 0 as the trace id (as `collect_ids` does) and column 1 with the same Arrow downcast Q1's `trace_targets` uses for `start_time`.

**Tests:**
- `reads/deletions.rs`:
  - the existing `filter_trace_ids` tests now check the start times too;
  - a new test, `filter_trace_ids_returns_start_times`.
- `reads/targets.rs`:
  - `filter_span_targets_follow_the_span_list`: same filter and window as a `SpanListRequest` over the same data, the same span ids, and trace starts attached;
  - `filter_span_targets_stop_at_the_limit`: a limit of 2 over 5 spans gives 2;
  - `filter_span_targets_dedup_copies`: a span ingested twice gives one target;
  - `filter_span_targets_exclude_eval_and_honour_experiments`;
  - `filter_session_targets_follow_the_session_list`: the sessions the list shows for the filter;
  - `filter_session_targets_use_the_earliest_visible_trace`: a session with one trace inside the window and an older one outside gets the older start as `start_time`, both trace ids, and is matched because of the in-window trace;
  - `filter_session_targets_stop_at_the_limit` (and `capped` is true);
  - `filter_session_targets_drop_unanswered_sessions`: a session with only an evaluation trace is matched by the scan but absent from the answer;
  - `filter_span_targets_use_the_trace_start`: a span inside the window whose trace starts before `from` gets the trace's start as `target_time`;
  - `filter_span_targets_capped_counts_before_drops`.
- The span list's and model tests' existing tests pass unchanged.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-storage reads::`
- `cargo test -p cetra-storage --no-run` (compiles the model tests); then run the model test once with the default case count: `cargo test -p cetra-storage storage_matches_the_model`.
- `cargo test -p cetra-server --lib deletion routes::deletions`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Return target times from the filter scans`

---

### R2 — The matching routes

**Goal.** `POST …/items/matching/preview` and `POST …/items/matching`, documented, with fixtures, specs and tests.

**Edit:**
- `crates/server/src/api/annotation_queues.rs`
- `crates/server/src/routes/annotation_queues.rs`
- `crates/server/src/api/openapi.rs`
- `crates/server/src/api_examples.rs` and `crates/server/tests/api_examples.rs`
- `fixtures/api/requests/annotation-queue-matching.json`, `fixtures/api/static/annotation-queue-matching-preview.json` and `fixtures/api/static/annotation-queue-matching-added.json` (new), `fixtures/api/manifest.json` and `fixtures/api/README.md` (if it lists files or ids)
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (generated)
- `docs/spec/09-api.md`
- `CHANGELOG.md`

**Steps.**
1. **Types** (derives as M2's bodies, plus `Serialize`):
   ```rust
   pub struct QueueMatchingRequest {
       #[serde(default, skip_serializing_if = "Option::is_none")] pub filter: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub search: Option<String>,
       pub from: TimeParam,
       pub to: TimeParam,
       #[serde(default)] pub include_experiments: bool,
   }
   pub struct QueueMatchingPreview { pub matched: u64, pub capped: bool }
   pub struct QueueMatchingAdded { pub added: u64, pub existing: u64, pub blocked: u64, pub capped: bool }
   ```
2. **Routes**, both perm `ManageQueues` with a 64 KiB body limit, path parameters `{project}` and `{id}`:
   - `POST P/{id}/items/matching/preview` → `preview_matching`, `200 QueueMatchingPreview`;
   - `POST P/{id}/items/matching` → `add_matching`, `201 QueueMatchingAdded`.
3. Constant `pub(crate) const MAX_MATCHING: usize = 10_000;` in the routes file.
4. **Shared preparation**, a private fn used by both handlers. Steps, in order:
   1. 404 when the queue is absent.
   2. `filter` and `search` are trimmed, and blank becomes `None` (mirror `non_blank` in `routes/deletions.rs`).
   3. A `search` on a session queue is `400` on `search`, message `"sessions have no search"`.
   4. `check_search` (`routes/read.rs`) applies to `search`.
   5. The filter text is passed to storage as text; storage parses it, and a bad filter comes back as the `RunError` that already maps to `400 invalid_filter` (as in `preview_deletion`).
   6. `TimeWindow::new(from.ns(), to.ns())?` (400 when `from >= to`, as deletions).
   7. Open the context with `routes::read::open_query(&state, org, project, None)`, which applies the data window. Never use `Visibility::exempt()` here.
   8. Run the kind's scan with the cap `cap` (a parameter of this preparation function; the handlers pass `MAX_MATCHING`) and `READ_TIMEOUT`:
      - traces: `filter_trace_ids` with a `FilterDeletionRequest { filter, search, window, include_eval: false, include_experiments, limit: cap + 1, timeout }`; `capped = len > cap`, truncated to `cap`;
      - spans: `filter_span_targets` with `limit: cap`; `capped` from its answer;
      - sessions: `filter_session_targets` with `limit: cap`; `capped` from its answer.
   9. Result: `Vec<(NewItem, Vec<TraceId>)>` (each target with its `target_time`, and the trace ids the guard checks: the trace id for trace and span items, `SessionTarget::trace_ids` for sessions), plus `capped`.
5. **`preview_matching`** answers `{matched: min(len, MAX_MATCHING), capped}`.
6. **`add_matching`** follows these steps:
   1. Reuse the guard-and-insert part of M2's `add_items`. If it is inline, extract it into a private fn in the same file with this contract: `async fn insert_found(state: &AppState, project: ProjectId, queue: Uuid, added_by: (AddedByKind, Uuid), found: Vec<(NewItem, Vec<TraceId>)>) -> Result<InsertFound, ApiError>` with `struct InsertFound { added: u64, existing: u64, blocked: Vec<usize> }` (indexes into `found`), mapping `ItemsInsert::TooMany` to `ApiError::TooManyItems`. Make `add_items` call it, so the guard runs inside the insert transaction for both.
   2. `blocked` is the number of blocked targets.
   3. `TooMany` → `422 too_many_items`.
   4. Log `tracing::info!(project_id, queue_id, added, existing, blocked, capped, "annotation queue items added by filter")`.
7. **OpenAPI** operation ids:
   - `preview_annotation_queue_matching` (responses `200`, `400, 401, 403, 404, 413, 415, 429, 503`);
   - `add_annotation_queue_matching` (`201`, plus `422`);
   - descriptions as M2's `ManageQueues` routes, noting the 10 000 cap and that evaluation traces are never matched.

   Run `just openapi`.
8. **Fixtures:**
   - the request `requests/annotation-queue-matching.json` (a traces filter, `from` and `to`);
   - the static answers;
   - manifest rows for both operations (`example` for the preview `200`, `example_created` for the add);
   - the request row, deserialize and roundtrip arms in `tests/api_examples.rs`;
   - then `just api-examples`.
9. **Spec:** add the two rows to `docs/spec/09-api.md` § Annotation queues: "the queue kind's list filter body (`filter`, `search` except sessions, `from`, `to`, `include_experiments`); preview `{matched, capped}`; add `201 {added, existing, blocked, capped}`; at most 10 000 per call, never evaluation traces, within the data window (D-AQ-8)".
10. **`CHANGELOG.md`** § `[Unreleased]` § Added: "Add everything a trace, span or session filter matches to an annotation queue, up to 10 000 per call."

**Tests** (in `routes/annotation_queues.rs`):
- `preview_and_add_matching_traces`:
  - commit 3 traces in the range and 1 outside;
  - preview answers `matched 3, capped false`;
  - add answers `added 3`;
  - a second add answers `existing 3`.
- `matching_respects_the_filter_and_search`: a filter on a span attribute narrows the trace match the way the traces list does.
- `matching_spans_and_sessions`: a span queue matches the spans of a filter with their trace start; a session queue matches sessions with their earliest trace start.
- `session_queue_refuses_search`: `400` on `search`.
- `matching_is_capped`: call the preparation function directly with `cap = 2` over 3 matching traces (each kind): it returns 2 targets and `capped = true`. Then check the handlers' mapping to `{matched: 2, capped: true}` through the same function with a small unit test of the answer builder (extract it if inline).
- `matching_skips_guarded_targets`: a guarded trace counts in `blocked`, not `added`.
- `matching_never_adds_evaluation_traces`.
- `matching_respects_the_data_window`: with `data_access_days` set, an old trace is not matched.
- `matching_permissions`: a viewer gets `403`; an `sk` write key works (`ManageQueues` allows `write` keys, D-AQ-11 and M2 Q4a).
- `bad_filter_is_400_invalid_filter`; `from >= to` is `400`.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-server --lib routes::annotation_queues`
- `cargo test -p cetra-server --test openapi --test api_examples`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add annotation queue items by filter`
