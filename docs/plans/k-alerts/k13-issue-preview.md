# K13 — Preview of issue occurrence rules

The alert editor and the rule panel draw a preview of the rule's metric per window, with the threshold line (D-K-13), and the rule list draws the same series as a small trend. `issue_occurrences` rules (D-IC-20) show "No preview for issue occurrences." and a dash in the list, because the dashboard widget query leaves issue occurrences out of its `scores` source (spec 06 § Issues), and the only occurrence series, `GET /issues/{id}/occurrences`, is per UTC day and per issue. This plan adds a read endpoint that counts a rule's occurrences per window-wide bucket, with the evaluator's own name resolution, and draws it in the preview and the trend.

**Prerequisites:** `main` at or after `ac0dfcb`. Track K and track IC are done.

**Read first:**
- `docs/spec/07-alerts.md` § Rule model (the `issue_occurrences` row, its bullets, and the "web preview" bullet) and § Evaluation loop, bullet **Issue names**.
- `docs/spec/06-*.md` § Issues, paragraphs **Occurrences** and **Counts**.
- `docs/spec/09-*.md`, the alert-rules row (line ~216) and the Issues table rows (lines ~227–239).
- `docs/spec/11-*.md` line ~228 (track K, the alerts pages).
- `docs/mvp-decisions.md` D-K-13 and D-IC-20.
- `crates/storage/src/reads/scores.rs` lines 25–180 (`score_name_counts`).
- `crates/server/src/issues/read.rs` `name_counts` and `tallies`.
- `crates/server/src/routes/issues.rs` `get_issue_occurrences` (≈ line 1636): the handler to imitate.
- `crates/server/src/meta/issue_links.rs` `issue_scope_ids`, `source_scope_ids`.
- `crates/server/src/alerts/evaluate.rs` `measure_issue_rule` (≈ line 318): the evaluator's count.
- `crates/server/src/routes/alert_rules.rs` (routes, `field_error`).
- `crates/server/src/api/alerts.rs` (`AlertWindow`), `crates/server/src/api/issues.rs` (`IssueOccurrenceParams`, `IssueDay`), `crates/server/src/api/annotation_queues.rs` ≈ line 220 (an `IntoParams` struct with an enum field).
- `crates/server/src/api/openapi.rs` `alert_rule_paths` (≈ line 6064) and the `get_issue_occurrences` operation (≈ line 3966).
- `crates/server/src/api_examples.rs` (the `get_issue_occurrences` example ≈ line 1418), `crates/server/tests/api_examples.rs` (the `roundtrip` list ≈ line 484), `fixtures/api/README.md`.
- `web/src/components/alerts/rule-preview.tsx`, `rule-trend.tsx`, `web/src/lib/alerts.ts` (`previewParams`, `previewRows`, `previewQuery`), `web/src/api/queries-alerts.ts`, `web/src/lib/metrics.ts` (`ChartRow`, `epochMs`).
- `web/src/routes/$org/$project/alerts/new.test.tsx` (the issue occurrence tests ≈ lines 440–500), `web/src/lib/alerts.test.ts` ≈ line 575.
- `website/src/content/docs/docs/platform/alerts.mdx`.

## What it is, and what it is not

When it is done:
- `GET /api/v1/projects/{project}/alert-rules/issue-preview?issue_id=…&window=…` (or `source_id=…`) answers the occurrence count of the issue (with its merge target and the issues merged into it) or of the source's counted issues, per bucket as wide as the window, over the same range the other previews use (24 h for `15m` and `1h`, 3 days for `6h`, 14 days for `24h`). A bucket with no occurrence is `0`.
- The alert editor's preview and the rule panel draw that series with the threshold line, for both scopes. The rule list's trend draws it too.
- Spec 07, 09 and 11, `docs/mvp-decisions.md` (D-K-22), the CHANGELOG and the alerts docs page say so.

Out of scope:
- A server preview for the other metrics: they keep the widget query.
- `latency_p95` with `target: llm_span` keeps "No preview".
- The docs screenshots: the website session recaptures them.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-K-13 | The preview's buckets are as wide as the window; ranges 24 h (≤ 1 h), 3 days (`6h`), 14 days (`24h`); threshold drawn as a line |
| D-IC-20 | `issue_occurrences` counts live `issue:<id>` scores of the names resolved now: issue → target + merged; source → issues neither ignored nor merged + their merged issues; windows `15m` or more |
| D-K-22 (new, this plan) | The issue occurrence preview is a server read, `GET …/alert-rules/issue-preview`, with the evaluator's name resolution. The user chose it over extending `GET /issues/{id}/occurrences` with a bucket plus a source variant, and over a per-day, issue-only preview without a server change (2026-11-25) |

User's answer (2026-11-25): "New preview endpoint" — `GET /projects/{p}/alert-rules/issue-preview?issue_id|source_id&window → {from, to, bucket, points:[{time, count}]}`, names resolved like the evaluator, window-wide buckets over the other previews' ranges, empty buckets 0, viewer role, storage's `score_name_counts` generalised from `by_day` to a bucket width.

## Versions

No new dependency.

## Phases

```
P1 storage + server callers ──► P2 endpoint, OpenAPI, fixtures, specs 07/09, D-K-22 ──► P3 web + spec 11 ──► P4 docs page + CHANGELOG
```

Sequential: P2 uses P1's request field, P3 uses P2's generated `schema.gen.ts`, P4 describes P3's UI.

| Phase | Files |
|---|---|
| P1 | `crates/storage/src/reads/scores.rs`, `crates/server/src/issues/read.rs`, `crates/server/src/routes/issues.rs` |
| P2 | `crates/server/src/api/alerts.rs`, `crates/server/src/routes/alert_rules.rs`, `crates/server/src/api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `crates/server/tests/openapi.rs`, `fixtures/api/static/alert-issue-preview.json`, `fixtures/api/manifest.json`, `fixtures/api/README.md`, `docs/spec/07-alerts.md`, `docs/spec/09-*.md`, `docs/mvp-decisions.md`, `docs/roadmap.md`, `docs/plans/k-alerts/README.md` |
| P3 | `web/src/api/queries-alerts.ts`, `web/src/lib/alerts.ts`, `web/src/lib/alerts.test.ts`, `web/src/components/alerts/rule-preview.tsx`, `web/src/components/alerts/rule-trend.tsx`, `web/src/routes/$org/$project/alerts/new.test.tsx`, `web/src/routes/$org/$project/alerts/index.test.tsx`, `docs/spec/11-*.md` |
| P4 | `website/src/content/docs/docs/platform/alerts.mdx`, `CHANGELOG.md` |

No build or test runs in a phase except the touched tests named in its **Done when**; the full `just check` runs once at the end of the plan (by the coordinating session).

---

### P1 — Count scores per bucket of any width

**Goal:** `score_name_counts` buckets by any positive width instead of only UTC days.

**Edit:** `crates/storage/src/reads/scores.rs`, `crates/server/src/issues/read.rs`, `crates/server/src/routes/issues.rs`.

**Steps:**
1. In `scores.rs`, replace the field `by_day: bool` of `ScoreNameCountsRequest` with:
   ```rust
   /// `Some(width)`: one row per (name, bucket), each bucket `[k·width, (k+1)·width)` ns since
   /// the epoch (`86_400_000_000_000` gives UTC days); `None`: one row per name for the whole
   /// window. A width must be positive.
   pub bucket_ns: Option<i64>,
   ```
   Update the doc of `ScoreNameCount::bucket_start` to: "Start of the bucket (ns), or `window.from` when `bucket_ns` is `None`."
2. In `score_name_counts`, after the names-count check and before the empty-names early return, refuse `Some(width)` with `width <= 0`: `RunError::InvalidRequest("the bucket width must be positive".to_owned())`. Add `# Errors` wording: "…or a bucket width that is not positive".
3. In `counts`, replace `if request.by_day { … lit(DAY_NS) … }` with `if let Some(width) = request.bucket_ns { keys.push((created.clone() - created.clone() % lit(width)).alias(BUCKET)); order.push(...) }`. In `counts_of`, `offset = usize::from(request.bucket_ns.is_some())`, and the bucket column is read when `bucket_ns.is_some()`. Remove `DAY_NS` from `scores.rs` if nothing else in the file uses it (tests may keep a local `DAY`).
4. Update the doc comment of `score_name_counts` ("in the day or the window" → "in each bucket or in the window").
5. In `crates/server/src/issues/read.rs`, change `name_counts`'s parameter `by_day: bool` to `bucket_ns: Option<i64>` and pass it through. `tallies` passes `None`.
6. In `crates/server/src/routes/issues.rs`, `get_issue_occurrences` passes `Some(DAY_NS)`. Use an existing server constant for a day in ns if `routes/issues.rs` already imports one; otherwise add `const DAY_NS: i64 = 86_400_000_000_000;` at the top of that file. Grep every other caller of `name_counts` and `ScoreNameCountsRequest` (`grep -rn "name_counts\|ScoreNameCountsRequest" crates/`) and update them the same way (`true` → `Some(DAY_NS)`, `false` → `None`).

**Tests** (in `scores.rs`'s test module, next to `score_name_counts_by_day`):
- Rename the existing `by_day: true` / `false` uses to `bucket_ns: Some(DAY)` / `None`; their expectations do not change.
- `score_name_counts_by_hour`: with `counted_scores(day1, day2)` and `bucket_ns: Some(HOUR)` (define `const HOUR: i64 = 3_600_000_000_000;` in the test module if absent), `issue:a`'s rows are at `day1` (count 2: `day1+100` and `day1+200` fall in the same hour) and `day2` (count 1), `issue:b` at `day1` (count 1). Then build a second list from `counted_scores(day1, day2)` with score 2 (`day1 + 200`) moved to `day1 + HOUR + 5` (set its `created_at`, and its id timestamp the way `score_row` does if the id encodes it), and assert `issue:a`'s rows are `day1` count 1, `day1 + HOUR` count 1, `day2` count 1.
- `score_name_counts_refuses_a_non_positive_bucket`: `Some(0)` and `Some(-1)` give `RunError::InvalidRequest`, also with an empty `names` list.

**Done when:** `cargo test -p cetra-storage --lib reads::scores` and `cargo test -p cetra-server --lib routes::issues` pass; `cargo clippy -p cetra-storage -p cetra-server --all-targets -- -D warnings` is clean.

**Commit:** `Count scores per bucket of any width`

---

### P2 — Issue occurrence preview endpoint

**Goal:** `GET /api/v1/projects/{project}/alert-rules/issue-preview` answers a rule's occurrence counts per window-wide bucket.

**Edit:** `crates/server/src/api/alerts.rs`, `crates/server/src/routes/alert_rules.rs`, `crates/server/src/api/openapi.rs`, `crates/server/openapi.json` (generated), `web/src/api/schema.gen.ts` (generated), `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `crates/server/tests/openapi.rs`, `fixtures/api/static/alert-issue-preview.json` (new), `fixtures/api/manifest.json` (generated), `fixtures/api/README.md`, `docs/spec/07-alerts.md`, `docs/spec/09-*.md`, `docs/mvp-decisions.md`, `docs/roadmap.md`, `docs/plans/k-alerts/README.md`.

**Contracts** (in `crates/server/src/api/alerts.rs`, next to the other alert types; derive the traits the neighbouring types derive; copy the `IntoParams` attributes from `IssueOccurrenceParams` and, for the enum field, from the `status: Option<QueueItemStatus>` field of `api/annotation_queues.rs`):

```rust
/// Query parameters of `GET …/alert-rules/issue-preview` (D-K-22). Spec 09 § Evaluation & alerts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct IssueAlertPreviewParams {
    /// The issue counted, with its merge target and the issues merged into that target.
    /// Exactly one of `issue_id` and `source_id`.
    #[serde(default)]
    pub issue_id: Option<Uuid>,
    /// The source whose issues are counted, except the ignored and merged ones (with the issues
    /// merged into each). Exactly one of `issue_id` and `source_id`.
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// The rule's window: `15m`, `1h`, `6h` or `24h`. It is the bucket width.
    pub window: AlertWindow,
}

/// Occurrence counts of an `issue_occurrences` rule, per bucket as wide as its window (D-K-22).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueAlertPreview {
    /// Start of the range, inclusive. JSON is RFC 3339.
    #[serde(with = "time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    /// End of the range, exclusive (the request time).
    #[serde(with = "time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub to: i64,
    /// The window, which is the bucket width.
    pub window: AlertWindow,
    /// One per bucket that overlaps `[from, to)`, oldest first, with no gap.
    pub points: Vec<IssueAlertPoint>,
}

/// One bucket of an [`IssueAlertPreview`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueAlertPoint {
    /// Start of the bucket: a multiple of the window since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub time: i64,
    /// Live occurrences created in the bucket and in `[from, to)`; 0 when none.
    pub count: u64,
}
```

The range constants (in `routes/alert_rules.rs`):

```rust
const HOUR_NS: i64 = 3_600_000_000_000;
const DAY_NS: i64 = 24 * HOUR_NS;

/// The preview range of a window (D-K-13): 24 h up to `1h`, 3 days for `6h`, 14 days for `24h`.
fn preview_span_ns(window: AlertWindow) -> i64 {
    match window {
        AlertWindow::M1 | AlertWindow::M5 | AlertWindow::M15 | AlertWindow::H1 => DAY_NS,
        AlertWindow::H6 => 3 * DAY_NS,
        AlertWindow::H24 => 14 * DAY_NS,
    }
}
```
(Reuse existing constants of the file if it already has them.)

**Steps:**
1. Add the three types above.
2. In `routes/alert_rules.rs`, add a `RouteDef`: `Method::GET`, path `"/api/v1/projects/{project}/alert-rules/issue-preview"`, `Access::Project { perm: ProjectPerm::View, session_only: false }`, `documented: true`, `handler: get(issue_preview)`. Put it before the `/alert-rules/{id}` routes. If the router refuses the static segment next to `{id}` or routes `issue-preview` to `get_rule`, stop and report; do not rename the path.
3. Handler `issue_preview(State(state): State<AppState>, Extension(access): Extension<ProjectAccess>, error::Query(params): error::Query<IssueAlertPreviewParams>) -> Result<error::Json<IssueAlertPreview>, ApiError>`, with `let project = access.project.id; let org = access.project.org_id;` as in `get_issue_occurrences`. New imports in `alert_rules.rs`: `std::collections::BTreeMap`, `cetra_storage::TimeWindow` (the path `routes/issues.rs` imports it from), `crate::meta::issue_links::{issue_scope_ids, source_scope_ids}`, `crate::issues::read::name_counts`, and `IssueAlertPreview`, `IssueAlertPreviewParams`, `IssueAlertPoint`, `AlertWindow` from `crate::api::alerts`. Steps, in this order:
   1. Neither or both of `issue_id` / `source_id` → `field_error("issue_id", "give exactly one of issue_id and source_id")`.
   2. `window` is `M1` or `M5` → `field_error("window", "issue occurrences need a window of 15m or more")` (copy that exact string; the rule validation has it inline in `alerts/condition.rs`).
   3. Resolve the ids with `issue_scope_ids(state.meta.reader(), project, id)` or `source_scope_ids(...)` from `crate::meta::issue_links`. `Ok(None)` → `ApiError::NotFound` (a unit variant). A `MetaError` converts with `?` as in the other handlers.
   4. `to = state.clock.now_ns()`, `start = to - preview_span_ns(window)`, `from = start - start.rem_euclid(width)` (`width = window.ns()`; the range starts at the window boundary at or before `start`, so every bucket but the last is full). Build the window with `TimeWindow::new(from, to)?` (its `RunError` converts to `ApiError` with `?`).
   5. Names: `ids.iter().map(|id| format!("issue:{id}"))`. If empty, skip the read (all counts 0). Otherwise `let ctx = open_query(&state, org, project, None).await?;` and `crate::issues::read::name_counts(&ctx, &names, window, Some(width)).await?` (make it `pub(crate)` reachable if it is not).
   6. Sum `row.count` per `row.bucket_start` into a `BTreeMap<i64, u64>` with `saturating_add`.
   7. Points: `let mut time = from;` then, while `time < to`, push `IssueAlertPoint { time, count: map.get(&time).copied().unwrap_or(0) }` and set `time += width`.
   8. Answer `IssueAlertPreview { from, to, window: params.window, points }`.
   axum's router prefers the static `issue-preview` segment over `{id}`, so the route does not collide with `get_rule`.
4. OpenAPI (`alert_rule_paths` in `api/openapi.rs`): an operation `operation("get_alert_issue_preview", "alerts", "Preview an issue occurrence rule")` with description "Any project member can read. Counts the live occurrences an `issue_occurrences` rule with this scope and window would count, per bucket as wide as the window: from the window boundary at or before 24 hours ago (`15m`, `1h`), 3 days ago (`6h`) or 14 days ago (`24h`), to now. Exactly one of `issue_id` and `source_id`; windows under 15 minutes are 400. A missing issue or source is 404.", parameters `with_query::<IssueAlertPreviewParams>(vec![project_param()])`, response `responses("200", json_of::<IssueAlertPreview>("The counts per bucket."), READ_ERRORS)`. Add `.path(format!("{PROJECT}/alert-rules/issue-preview"), PathItem::new(HttpMethod::Get, issue_preview))`. Register the schemas the way `IssueOccurrences` is (`set.insert::<…>()` ≈ line 622, next to the alert schemas ≈ lines 593–595), for `IssueAlertPreview` and `IssueAlertPoint`, and add `IssueAlertPreview`, `IssueAlertPoint`, `IssueAlertPreviewParams` to the `super::alerts::{…}` import (≈ line 38).
   In `crates/server/tests/openapi.rs`: add the operation's row to the operation table next to `get_alert_rule` (≈ lines 536–562, same shape as its neighbours: `GET`, `{PROJECT}/alert-rules/issue-preview`, `get_alert_issue_preview`), add `"get_alert_issue_preview"` to `READ_OPERATIONS` (≈ line 1227) so its expected errors are `READ_ERRORS`'s, and add it to every other list in that file that names `get_issue_occurrences` (≈ lines 809 and 1245) when the list's meaning applies to it (read the list's comment).
5. Run `just openapi` (it needs Node on `PATH`: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first) to regenerate `crates/server/openapi.json` and `web/src/api/schema.gen.ts`.
6. Example: write `fixtures/api/static/alert-issue-preview.json` by hand (canonical JSON as the other static files: 2-space indent, keys in struct order, trailing newline): `window` `"1h"`, `from` `"2026-01-01T12:00:00.000000000Z"`, `to` `"2026-01-02T12:00:00.000000000Z"`, and 24 points from `2026-01-01T12:00` to `2026-01-02T11:00` hourly (the range is hour-aligned, so the point at `to` is not included); counts `0` except `2026-01-01T15:00` → 2, `2026-01-02T03:00` → 5, `2026-01-02T04:00` → 4, `2026-01-02T09:00` → 1. Add an `example("get_alert_issue_preview", BTreeMap::new(), BTreeMap::new(), "static/alert-issue-preview.json", "application/json")` in `api_examples.rs` next to the alert examples. In `tests/api_examples.rs`: `roundtrip::<IssueAlertPreview>("static/alert-issue-preview.json")` in the static list (≈ line 484), the arm `"get_alert_issue_preview" => roundtrip::<IssueAlertPreview>(&example.file),` in the per-operation `match` (next to `get_issue_occurrences` ≈ line 853), and `IssueAlertPreview` in the file's `use` list; and a row in `fixtures/api/README.md`'s identifier table or layout notes: "`static/alert-issue-preview.json`: hand, the hourly occurrence counts of an `issue_occurrences` rule with a `1h` window". Run `just api-examples` to regenerate `manifest.json`.
7. Specs and decisions:
   - `docs/mvp-decisions.md`, track K section after D-K-21: `| D-K-22 | Issue occurrence preview | **An issue occurrence rule's preview is a server read.** `GET …/alert-rules/issue-preview` takes the rule's scope (`issue_id` or `source_id`) and window, resolves the names as the evaluator does (D-IC-20), and answers the live occurrence count per bucket as wide as the window over D-K-13's ranges, with 0 for an empty bucket. The widget query cannot draw it: its `scores` source leaves issue occurrences out. Rejected: a bucket parameter on `GET /issues/{id}/occurrences` with a source variant (two endpoints, and the issue page's day series would share the code), and a per-day, issue-only preview (days do not match the window, so the threshold line misleads). |` Keep the table's column layout of the neighbouring rows. In D-K-13's row, append: "Issue occurrence rules preview through their own read instead (D-K-22)."
   - `docs/plans/k-alerts/README.md`: add K13 to its list of plans in the same format as the K12 entry, pointing to `k13-issue-preview.md`.
   - `docs/roadmap.md` track K row: decision range `D-K-1–D-K-22`, and append "K13: the issue occurrence preview (D-K-22)." to its status cell.
   - `docs/spec/07-alerts.md`, the "web preview" bullet: replace with "The web preview (the alert form, the rule panel and the rule list) draws the dashboard's trace error rate, cost, p95 latency and score average through the widget query (same source, time column and filter). It may therefore differ from the value the evaluator computes for score measures (7-day trace lookback, D-K-17) and for `llm_span` latency (end-time window, D-K-18); the web preview does not draw `llm_span` latency at all. An `issue_occurrences` rule's preview reads `GET …/alert-rules/issue-preview` (09), which counts the names the evaluator would resolve now, per bucket as wide as the window (D-K-22)."
   - `docs/spec/09-*.md`, the alert-rules row: add after the `GET /alert-events` sentence: "`GET /alert-rules/issue-preview?issue_id=|source_id=&window=` (role viewer, D-K-22) answers `{from, to, window, points}`: the live occurrences an `issue_occurrences` rule of that scope would count, per bucket as wide as the window (each `{time, count}`, `time` a multiple of the window since the epoch, 0 when empty, no gap), over 24 hours for `15m` and `1h`, 3 days for `6h` and 14 days for `24h`, ending now. Names are resolved as the evaluator does (07 § Issue names). Exactly one of `issue_id` and `source_id` (`400` on `issue_id`); `1m` and `5m` are `400` on `window`; a missing issue or source is `404`."

**Tests** (in `routes/alert_rules.rs`'s test module; write occurrences with `crate::test_support::issues::occurrence(app, seeded, source, issue, trace, at, user, span)` — `trace` is 32 lowercase hex digits such as `"a1000000000000000000000000000001"`, `at` is ns, `user` and `span` may be `None`; one occurrence per (trace, issue), so use a different trace per occurrence — and issues/sources with `put_issue`, `issue_row`, `put_source`, `source_row`, as the existing issue-rule tests there do. A merged issue is `let mut row = issue_row(...); row.merged_into = Some(target); row.state = IssueState::Merged;` before `put_issue`. `now` is `app.state.clock.now_ns()`; do not assume it is hour-aligned, compute expected buckets from it):
- `issue_preview_counts_per_window`: an open issue with occurrences 30 min, 90 min and 100 min before now; `window=1h` answers `window` `"1h"`, `to - from` = 24 h, 25 points (24 when `from` is on an hour boundary: assert `points.len()` equals the number computed from `from`/`to` as in step 3.7), every `time` a multiple of 1 h, consecutive points 1 h apart, the sum of counts 3, and the two occurrences 90 and 100 min ago in the same point unless an hour boundary falls between them (compute the expected buckets from the test clock instead of hard-coding).
- `issue_preview_counts_merged_issues`: issue B merged into A (set `merged_into` as the existing merged-issue tests do); occurrences on both; `issue_id=A` and `issue_id=B` both sum both.
- `issue_preview_of_a_source_skips_ignored_issues`: a source with an open issue (1 occurrence) and an ignored issue (1 occurrence); `source_id` sums 1.
- `issue_preview_of_an_empty_source_is_zero`: a source with no issue: every point is 0, the point count is as for the range.
- `issue_preview_refuses_bad_parameters`: neither id → 400 field `issue_id`; both → 400 field `issue_id`; `window=5m` → 400 field `window`; `window=2h` → 400; an unknown issue and an unknown source → 404.
- `issue_preview_ranges`: `6h` spans 3 days, `24h` spans 14 days with points at UTC midnights.
- `issue_preview_is_readable_by_a_viewer`: a viewer of the project gets 200 (use the existing viewer helper of this test module, as the `list_rules` viewer test does).
- `assert_logs_clean` as the module's other tests use it, if they do.

**Done when:** `cargo test -p cetra-server --lib routes::alert_rules`, `cargo test -p cetra-server --test api_examples`, `cargo test -p cetra-server --lib api_examples` and `cargo test -p cetra-server --lib openapi` pass; `cargo clippy -p cetra-server --all-targets -- -D warnings` is clean; `git status` shows `openapi.json`, `schema.gen.ts` and `manifest.json` regenerated.

**Commit:** `Add the issue occurrence preview endpoint`

---

### P3 — Draw the issue occurrence preview

**Goal:** the editor, the rule panel and the rule list draw an issue occurrence rule's counts with its threshold.

**Edit:** `web/src/api/queries-alerts.ts`, `web/src/lib/alerts.ts`, `web/src/lib/alerts.test.ts`, `web/src/components/alerts/rule-preview.tsx`, `web/src/components/alerts/rule-trend.tsx`, `web/src/routes/$org/$project/alerts/new.test.tsx`, `web/src/routes/$org/$project/alerts/index.test.tsx`, `docs/spec/11-*.md`.

**Contracts:**

```ts
// web/src/api/queries-alerts.ts
export function issueAlertPreviewQuery(
  org: string,
  project: string,
  scope: { issue_id: string } | { source_id: string },
  window: Schema<"AlertWindow">,
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/alert-rules/issue-preview", {
    params: { path: { project: projectRef(org, project) }, query: { ...scope, window } },
  });
}

// web/src/lib/alerts.ts
/** The scope and window of an issue occurrence preview, or why there is none: the form has no
 *  issue or source yet, or a window under 15 minutes. `undefined` for the other metrics. */
export function issuePreviewScope(
  form: Pick<RuleFormValues, "metric" | "issueScope" | "issueId" | "sourceId" | "window">,
):
  | { kind: "ready"; scope: { issue_id: string } | { source_id: string }; window: Window }
  | { kind: "missing"; message: string }
  | undefined;

/** The chart rows of an issue occurrence preview: one per point, `all` the count. */
export function issuePreviewRows(answer: Schema<"IssueAlertPreview"> | undefined): ChartRow[];
```
(`Schema` from `@/api/types`; `Window` is the type `previewParams` already takes. If `AlertWindow` is not the generated schema name, use the name `schema.gen.ts` has for the `window` query parameter.)

**Steps:**
1. `issuePreviewScope`: `metric !== "issue_occurrences"` → `undefined`. `window` `1m` or `5m` → `{kind: "missing", message: "Issue occurrences need a window of 15 minutes or more."}`. Scope `issue` with `issueId.trim() === ""` → `{kind: "missing", message: "Choose an issue to see the preview."}`; scope `source` with empty `sourceId.trim()` → `"Choose an issue source to see the preview."`. Else `ready` with the trimmed id under `issue_id` or `source_id`.
2. `issuePreviewRows`: `undefined` → `[]`; else `answer.points.map(p => ({ time: epochMs(p.time) ?? 0, all: p.count }))`, dropping points whose `epochMs` is `undefined` rather than using 0 (use `flatMap`).
3. Update `previewQuery`'s doc comment: issue occurrences are drawn from `issuePreviewRows`, not the widget query; it still returns `undefined` for them.
4. `rule-preview.tsx`:
   1. Extract from `PreviewBody` the part after the data is loaded into `function PreviewChart({ rows, values }: { rows: ChartRow[]; values: RuleFormValues })` that renders the `<figure>` (it calls `useTimeZone()` itself for the tick formatter; threshold from `conditionFromForm(values)`, `condition` from `previewCondition(values)`, same markup as today), and `function PreviewFrame({ body, caption }: { body: ReactNode; caption: string })` that renders the `Preview` heading, the body and the caption paragraph. `PreviewBody` uses both, with its current caption.
   2. In `RulePreview`, before `previewQuery`: `const issue = issuePreviewScope(values);` If `issue?.kind === "missing"`, render `PreviewFrame` with body `<p className="text-body text-muted-foreground">{issue.message}</p>` and the issue caption (step 4.4). If `ready`, render `<IssuePreviewBody org project values scope={issue.scope} window={issue.window} />`. Remove the `issue_occurrences` branch of the "No preview" message (the remaining message is the LLM span one).
   3. `IssuePreviewBody`: `useQuery({ ...issueAlertPreviewQuery(org, project, scope, window), staleTime: 60_000 })`. Pending → `<Skeleton className="h-40 w-full" />`. Error → `"The preview could not be loaded."` (as today), except a 404 problem (`problemOf(error)?.status === 404`, or the field the `Problem` type uses for the status; check `web/src/api/problem.ts`) → `"The issue or source no longer exists."`. Data → `PreviewChart` with `issuePreviewRows(data)`; zero counts are values, so there is no "No data in this range." state.
   4. Issue caption: "Each point is the occurrences over one window, as the rule counts them now: merged issues count with their target."
   5. The figure's aria label stays `Preview of the metric over the last <range>`.
5. `rule-trend.tsx`: split `Trend` so the drawing is `function TrendLine({ rule, values }: { rule; values: (number | null)[] })` (everything after `values` is computed, including the `Dash` when nothing is defined). `TrendChart`: if `issuePreviewScope(formFromRule(rule))?.kind === "ready"`, render `IssueTrend` which fetches `issueAlertPreviewQuery` with `staleTime: 60_000` and passes `issuePreviewRows(data).map(r => r.all ?? null)` to `TrendLine`, with the same pending and error handling as `Trend`. A `missing` scope renders `Dash`. Other metrics are unchanged.
6. `docs/spec/11-*.md` line ~228: replace "Its window choices start at 15 minutes, it has no filter, no minimum samples and no preview." with "Its window choices start at 15 minutes, and it has no filter and no minimum samples. Its preview and its trend in the list draw the occurrences per window from `GET …/alert-rules/issue-preview` (D-K-22); before an issue or source is chosen, the preview asks for one." and in the rules-list sentence keep the rest.

**Tests:**
- `alerts.test.ts`: `issuePreviewScope` for another metric (`undefined`), `5m` (missing, window message), empty issue id with spaces (missing), empty source id (missing), a ready issue (trimmed id, `issue_id` key), a ready source (`source_id` key). `issuePreviewRows` of `undefined` (`[]`) and of a 3-point answer with one zero count (`all: 0`, not `null`; `time` in ms). Keep the existing `previewQuery(... "issue_occurrences" ...)` test (still `undefined`).
- `new.test.tsx`: replace "says there is no preview for issue occurrences" with:
  - "asks for an issue before previewing issue occurrences": choose the metric; expect "Choose an issue to see the preview.".
  - "previews the occurrences of the chosen issue": serve the preview endpoint with `server.use(http.get(…/alert-rules/issue-preview, …))` returning `fixtures/api/static/alert-issue-preview.json`, capturing the request URL; choose the metric and an issue the way the existing issue-scope tests do; expect `figure` with name matching `/Preview of the metric/`, and the captured query has `issue_id=<the issue>` and `window=1h` (or the form's default window for this metric).
  - "says when the previewed issue no longer exists": the endpoint answers 404 with a problem body (as other tests build one); expect "The issue or source no longer exists.".
- `index.test.tsx`, "draws an issue occurrence rule's trend once the row is visible": copy the `IntersectionObserver` stub and trigger of "draws the trend with the threshold once the row is visible" (≈ lines 253–295); `server.use` an `http.get` of the alert-rules list answering one `issue_occurrences` rule (`condition: { metric: "issue_occurrences", issue_id: "01900000-0000-7000-8000-000000001c01", window: "1h", comparator: "gt", threshold: 3 }`, the other fields copied from `fixtures/api/static/alert-rule.json`), and an `http.get` of the preview answering `alert-issue-preview.json`; expect `img` named `Trend of <name> over the last 1h`.

No MSW handler or test setup change is needed: `web/src/mocks/manifest.ts` serves every manifest entry from `openapi.json`, and ranks `/alert-rules/issue-preview` ahead of `/alert-rules/:id`.

**Done when:** `cd web && pnpm exec vitest run src/lib/alerts.test.ts 'src/routes/$org/$project/alerts' src/components/alerts --maxWorkers=8` passes, and `pnpm exec tsc --noEmit -p .` and `pnpm exec biome check src` are clean (use the web package's own script names if they differ: see `web/package.json`). Delete `web/.vitest/output.json` if a run through RTK wrote it.

**Commit:** `Draw the issue occurrence preview`

---

### P4 — Docs page and CHANGELOG

**Goal:** users read that issue occurrence rules have a preview.

**Edit:** `website/src/content/docs/docs/platform/alerts.mdx`, `CHANGELOG.md`.

**Steps:**
1. `alerts.mdx`, in the paragraph after the line that says cetra refuses `1m` and `5m` for this metric (≈ line 40), add: "The preview next to the form counts the occurrences per window over the last day (3 days for a 6-hour window, 14 days for a 24-hour window), as the rule would count them now: an issue with the issues merged into it, or a source's issues except the ignored and merged ones." Follow `website/STYLE.md`; do not name internal decision IDs on the page.
2. `CHANGELOG.md` § `[Unreleased]` § `### Added`, append: "- The alert editor, the rule panel and the rule list chart an issue occurrence rule's occurrences per window with its threshold, through `GET /api/v1/projects/{project}/alert-rules/issue-preview`."

**Done when:** `cd website && pnpm exec node scripts/check-terms.mjs` passes (or the `check-terms` step of `just check-docs`, run alone).

**Commit:** `Document the issue occurrence preview`
