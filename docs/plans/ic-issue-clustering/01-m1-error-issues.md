# IC M1 · Error issues — plan

M1 delivers issue clustering for errors, end to end, with no LLM:
- an `error` issue source per project;
- error spans of completed traces fingerprinted into issues;
- an occurrence score per (trace, issue);
- a lifecycle with regression, and merge;
- the issues and issue-sources API;
- the Issues and Issue sources pages, issue chips on the trace view, and a "Has issue" filter;
- the `max_active_issue_sources` plan limit.

**Prerequisites:** M2.4 (fingerprint) is merged into this branch: M1 is cut from the M2 branch after M2.5.

**Read first:**
- `AGENTS.md`;
- `00-design.md`, all of it (decisions D-IC-1 to D-IC-17);
- `crates/server/src/eval/trigger.rs` (`refresh_rules`, `load`, `sweep_once`, `sweep_batch`, `read_candidates`, `settle`), `eval/pending.rs`, `eval/sampling.rs`;
- `crates/server/src/scores.rs` (`Writer`, `write`, `times`, `derive_id`, `is_v7`);
- `crates/server/src/routes/online_rules.rs`, `api/online_rules.rs`, `meta/online_rules.rs`, `eval/rule_limit.rs` (the template for sources);
- `crates/storage/src/reads/traces.rs` (`span_refresh`, `refresh_spans`), `reads/scores.rs` (`latest_scores`, `latest`, `score_list`), `reads/users.rs` (`scores_of`), `reads/experiments.rs` (`experiment_score_means`, the pattern for a new aggregate read);
- `crates/server/src/entitlements/mod.rs` (`Limits`), `crates/token/src/lib.rs` (`LimitsClaim`);
- the web online-rules pages (`web/src/routes/$org/$project/online-rules/`, `web/src/components/online-rules/`, `web/src/api/queries-online-rules.ts`), `web/src/components/shell/nav.ts`, `web/src/lib/shortcuts.ts`, `web/src/mocks/writes.ts`;
- code facts gathered for this plan (outside the repo, read-only): `ic-m1-code-facts.md` and `ic-m1-code-facts-2.md` in the design sessions' notes folder. The implementer does not need them; every fact used is restated here.

## What it is, and what it is not

When M1 is done, a member can:
- create an error source on a project, with an optional spans filter;
- see every distinct error of new traces become an issue within seconds of the trace completing;
- browse issues (open, resolved, ignored, all) with counts and sparklines, and open an issue with its daily chart, affected users, example traces and events;
- resolve, ignore, reopen, rename and merge issues; a resolved issue that recurs is reopened as regressed;
- filter the trace list to an issue's traces, and see issue chips on a trace.

Free orgs get one active source.

Out of scope:
- LLM kinds, candidates, embeddings and refit (M3);
- alerts, notifications, datasets and MCP (M4);
- annotation queues (M5).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IC-1 | Sources; `error` takes a `spans` filter, no sampling and no cap |
| D-IC-2 | Fingerprints; the `error_spans` read, at most 50 per trace |
| D-IC-3 | An issue belongs to one source; deleting a source deletes its issues |
| D-IC-9 | Merge: one level deep; counts read the whole name set; "View traces" uses ≤ 8 names |
| D-IC-10 | States, regression, events |
| D-IC-11 | Occurrence scores: name `issue:<id>`, source `issue`, `created_at` = trace start, not billed, hidden from score-name lists |
| D-IC-13 | `max_active_issue_sources`: Free 1, paid unlimited, counts enabled sources |
| D-IC-16 | Pages |
| D-IC-17 | Fingerprint bytes (M2.4's `cetra_eval::issue::fingerprint`) |
| D-I-8, D-I-9 | Pending traces and the completion sweeper |
| D-I-24 | Permissions (`ManageEvaluation` to manage, `View` to read) |
| D-K-14 | `403 plan_limit` check pattern |
| D97 | At most 8 score names per filter |

**Decided while writing this plan** (added to `00-design.md` and the decision log as D-IC-18 in M1.1):
- **The occurrence score id is a UUIDv7** whose 48-bit millisecond timestamp is the occurrence time and whose other bits come from SHA-256(`cetra.issue-occurrence.v1` ‖ project id (16 bytes) ‖ trace id (16) ‖ issue id (16)), with the version and variant bits set. `scores::write` then takes `created_at` from the id (the existing D-G-26 path) and a rewrite of the same (trace, issue) replaces the same row.
- **The occurrence time of an error issue is the `start_time` of the trace's earliest error span with that fingerprint**, clamped to `now`. It is stable across re-sweeps. When a late span of the same fingerprint starts earlier still, a second row is written for the trace, and the count is one too high (accepted, rare). Occurrences older than the project's retention are not written (`scores::write` refuses them).
- **Occurrences carry the trace's user and session**: from the first error span of the group (in `(start_time, span_id)` order) with a non-null `user_id`, and likewise for `session_id`.
- **`latest_scores` and the users' `scores_of` leave out source `issue`.** The list columns and briefs never see issue scores. The trace view gets its chips from `GET …/scores?trace_id=…&source=issue`, and the scores tab hides rows of source `issue`. The dashboard scores source leaves out source `issue` too.
- **No `occurrence_count` column.** Counts come from storage (`score_name_counts`), so deletions are reflected.
- **`issue_sources.kind` has no SQL CHECK**, so M3 adds kinds without rebuilding the table. The Rust enum is the only gate.
- **Regression compares trace times.** A trace that started before `resolved_at` but completes after it does not reopen the issue.

## Versions

No new dependency.

## Phases

| Phase | Edits (summary) | Depends on |
|---|---|---|
| M1.1 Score source `issue` | core enums, storage score reads, server convert, web labels, openapi, docs text, design doc and decision log | — |
| M1.2 Storage reads | `crates/storage/src/reads/{traces,scores,mod}.rs`, `lib.rs` | M1.1 |
| M1.3 Tables and store | migration `0023_issues.sql`, `meta/issues.rs`, `meta/mod.rs`, schema snapshot, spec 04 | — |
| M1.4 Plan limit | entitlements, config, token, CLI, OpenAPI, web plan page, docs | — |
| M1.5 Issue writer | `scores.rs` | M1.1 |
| M1.6a Tracked set | new `issues/sources.rs`, `issues/mod.rs`, `eval/trigger.rs` | M1.3 |
| M1.6b Fingerprints to issues | new `issues/errors.rs`, `issues/mod.rs`, `eval/trigger.rs` | M1.2, M1.6a |
| M1.6c Occurrence writes | `issues/errors.rs`, `eval/trigger.rs` | M1.5, M1.6b |
| M1.7 Sources API | `api/issues.rs`, `routes/issues.rs` (sources half), routes registration, openapi, fixtures, web mocks | M1.3, M1.4, M1.6 |
| M1.8a Issues reads | `api/issues.rs`, `routes/issues.rs`, `routes/alert_rules.rs`, openapi, fixtures | M1.7 |
| M1.8b Issues writes | `api/issues.rs`, `routes/issues.rs`, web mocks, and the dead-code attributes removed | M1.8a |
| M1.9a Web: Issues list | `web/src/routes/$org/$project/issues/index.tsx`, queries, search, nav, shortcuts | M1.8b |
| M1.9b Web: issue page | `web/src/routes/$org/$project/issues/$issueId.tsx`, `web/src/components/issues/**` | M1.9a |
| M1.10 Web: Issue sources pages | `web/src/routes/$org/$project/issue-sources/**`, `web/src/components/issue-sources/**` | M1.8 |
| M1.11 Web: trace chips and filter | trace view, scores tab, preset chips | M1.8 |
| M1.12 Docs and closeout | specs, website page, CHANGELOG, roadmap | all |

The phases run one after another, in one worktree.

**Rules for every phase** (repeat them in each brief):
- Follow the plan exactly. If a step is unclear, contradicts the code or cannot be done as written, stop and report instead of improvising.
- Touch only the phase's **Edit** files, plus files that are regenerated (`crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json`, insta snapshots) when the phase says so.
- No `unwrap()`/`expect()` outside tests unless a comment states the invariant. No payload text (error messages, titles, summaries) in logs; log ids and counts.
- Rust: compile and test only right before the commit, once. Web: while iterating, run only the touched tests: `pnpm --dir web exec vitest run <paths> --maxWorkers=8`.
- Before `just check` or any web command: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
- Commit once, with the given subject, when **Done when** passes. Stage only the phase's files (`git add <paths>`), never `git add -A`.
- **Dead code.** A new `pub(crate)` item that no non-test code uses yet fails clippy. Put `#![cfg_attr(not(test), allow(dead_code))]` at the top of a new module that is not used yet (`meta/issues.rs` in M1.3, `issues/source_limit.rs` in M1.4), and `#[cfg_attr(not(test), allow(dead_code))]` on a new item (`Writer::Issue`, `occurrence_id` in M1.5). M1.8b removes every such attribute added by M1 (grep `allow(dead_code)` in the files M1 created or changed), and its `just check-rust` proves nothing is left unused.

---

### M1.1 Score source `issue`

**Goal:** `ScoreSource::Issue` exists, nothing that lists score names shows issue scores, and D-IC-18 is recorded.

**Edit:**
- `crates/core/src/enums.rs`; `crates/core/tests/snapshots/enums.snap`;
- `crates/storage/src/reads/scores.rs`, `crates/storage/src/reads/users.rs`, `crates/storage/src/reads/widgets.rs`;
- `crates/storage/src/views/catalog.rs`;
- `crates/server/src/scores.rs` (only the `Writer::source` match is untouched: see step 3);
- `web/src/components/trace-columns.tsx`, `web/src/components/session-columns.tsx`, `web/src/components/trace/scores-tab.tsx`;
- `fixtures/api/static/schema.json`;
- regenerated: `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `website/src/content/docs/docs/reference/sql-schema.md`;
- `docs/spec/01-data-model.md`, `docs/spec/06-evaluation.md`, `website/src/content/docs/docs/evaluation/scores.mdx`;
- `docs/plans/ic-issue-clustering/00-design.md`, `docs/mvp-decisions.md`.

**Steps:**
1. `crates/core/src/enums.rs` `ScoreSource`: add the last variant `/// An issue occurrence written by an issue source (D-IC-11). Issue,` (wire name `issue`). Update `enums.snap` line `score_source: [api, sdk, human, end_user, llm_judge, check, webhook, issue]`.
2. Text copies:
   - `views/catalog.rs` (the `source` column description, around line 659) becomes "`api`, `sdk`, `human`, `end_user`, `llm_judge`, `check`, `webhook` or `issue`.";
   - `fixtures/api/static/schema.json` (around line 653) gets the same text;
   - `docs/spec/01-data-model.md` § Score `source` row adds `` \| `issue` ``;
   - `docs/spec/06-evaluation.md` § Scores, where it lists sources, adds "`issue`: an issue occurrence (track IC); never accepted from a client";
   - `website/.../evaluation/scores.mdx` (around line 19) adds `issue` with the same words.
3. `scores.rs`: no change to `Writer::source`: the `_ => None` arm already refuses `issue` from every existing writer. Add a unit test `issue_source_is_refused_from_clients`: a `Writer::Api` and a `Writer::Sdk` write with `source: Some(ScoreSource::Issue)` are both `400` with field `scores[0].source`. Copy the shape of the existing test that refuses another source.
4. Exclusions:
   - `reads/scores.rs` `latest()`: add the conjunct `col("source").not_eq(lit("issue"))` next to the existing filter (around line 268), with the comment `// Issue occurrences never show as score columns or briefs (D-IC-18).`;
   - `reads/users.rs` `scores_of`: the same conjunct in its filter (around lines 389–392);
   - `reads/widgets.rs`: where the frame for `WidgetSource::Scores` is first built (in `base()`), add the same conjunct;
   - each gets a test in its file: write one score of source `issue` and one of source `api` with the same target, and the read returns only the `api` one. Copy the closest existing test's setup.
5. Web:
   - `SOURCE_LABEL` in `trace-columns.tsx` and `session-columns.tsx`: add `issue: "Issue"`;
   - `scores-tab.tsx`: the rows shown are `scores.filter((score) => score.source !== "issue")`, with a test that a fixture row of source `issue` is not rendered.
6. Run `just openapi` and `just docs-gen`.
7. Design doc and decision log:
   - in `00-design.md`, add `### D-IC-18 Occurrence ids, hiding and counts` after D-IC-17, with the four bullets of this plan's "Decided while writing this plan";
   - in `docs/mvp-decisions.md` § Track IC, add the row: `| D-IC-18 | Occurrence ids and hiding | **An occurrence score id is a UUIDv7 stamped with the occurrence time (for errors, the start of the trace's earliest error span of that fingerprint, clamped to now), its other bits from SHA-256(cetra.issue-occurrence.v1 ‖ project ‖ trace ‖ issue)**, so \`created_at\` is when the problem happened and rewrites are idempotent; occurrences carry the trace's user and session; regression compares trace times. \`latest_scores\`, users' \`scores_of\` and the dashboard scores source leave out source \`issue\`; the trace view reads issue scores with \`source=issue\`. Issues have no stored count: storage counts their scores. \`issue_sources.kind\` has no SQL CHECK. |`.

**Tests:** steps 3–5. `enums.rs`'s round-trip test covers the new variant.

**Done when:** `just check-rust` and `just check-web` pass, and `just check-docs` passes (for `scores.mdx` and the regenerated `sql-schema.md`).

**Commit:** `Add the issue score source and hide issue scores from score lists`

---

### M1.2 Storage reads

**Goal:** the two reads M1 needs: error spans with their fields, and per-day counts for a list of score names.

**Edit:** `crates/storage/src/reads/traces.rs`, `crates/storage/src/reads/scores.rs`, `crates/storage/src/reads/mod.rs`, `crates/storage/src/lib.rs` (re-exports).

**Contracts:**
```rust
// reads/traces.rs
/// Most error spans `error_spans` returns for one trace (D-IC-2).
pub const MAX_ERROR_SPANS_PER_TRACE: usize = 50;

/// Error spans of the given traces that match a span filter, for error issue sources (D-IC-2).
#[derive(Debug, Clone, Copy)]
pub struct ErrorSpansRequest<'a> {
    /// Filter text over `spans`. Empty or whitespace means no filter. `status = 'error'` is
    /// always added.
    pub filter: Option<&'a str>,
    /// Trace ids, 32 hex digits, either case. At most [`MAX_REFRESH_IDS`].
    pub trace_ids: &'a [&'a str],
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// The columns `error_spans` returns, in this order.
pub const ERROR_SPAN_COLUMNS: &[&str] = &[
    spans::TRACE_ID, spans::SPAN_ID, spans::START_TIME, spans::SERVICE_NAME, spans::NAME,
    spans::STATUS_MESSAGE, spans::EVENTS, spans::USER_ID, spans::SESSION_ID,
];

pub async fn error_spans(ctx: &QueryContext, request: ErrorSpansRequest<'_>)
    -> Result<Vec<RecordBatch>, RunError>;

// reads/scores.rs
/// Most names one `score_name_counts` takes (D-IC-9).
pub const MAX_COUNT_NAMES: usize = 1_000;

#[derive(Debug, Clone, Copy)]
pub struct ScoreNameCountsRequest<'a> {
    /// Score names; empty returns no rows. At most [`MAX_COUNT_NAMES`].
    pub names: &'a [String],
    /// `created_at ∈ [from, to)`.
    pub window: TimeWindow,
    /// `true`: one row per (name, UTC day); `false`: one row per name for the whole window.
    pub by_day: bool,
    pub timeout: Duration,
}

/// Live scores of one name, in one day or in the whole window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreNameCount {
    pub name: String,
    /// Start of the UTC day (ns), or `window.from` when `by_day` is false.
    pub bucket_start: i64,
    /// Live scores.
    pub count: u64,
    /// Distinct non-null `user_id`s.
    pub users: u64,
    /// Greatest `created_at` among them.
    pub last_created_at: i64,
}

pub async fn score_name_counts(ctx: &QueryContext, request: ScoreNameCountsRequest<'_>)
    -> Result<Vec<ScoreNameCount>, RunError>;
```
Re-export all of them from `reads/mod.rs` and the crate root, next to `span_refresh` and `latest_scores`.

**Steps:**
1. `error_spans`: validate like `span_refresh` (more than `MAX_REFRESH_IDS` ids is `InvalidRequest`; parse the filter with `FilterView::Spans`; ids through `hex_id`; empty ids return `Ok(vec![])`). Then run, under `tokio::time::timeout`, a copy of `refresh_spans`'s body with three changes:
   - `request.strict_exclusions` is always true: use `strict_exclusions(ctx, FilterView::Spans, Some(ids))`;
   - add the conjunct `col(spans::STATUS).eq(lit("error"))`;
   - rank by `(start_time, span_id)` per trace, keep `rn <= MAX_ERROR_SPANS_PER_TRACE`, and select `ERROR_SPAN_COLUMNS`.

   Share the ranking code with `refresh_spans` by extracting a private `fn rank_per_trace(frame, per_trace: u64) -> Result<DataFrame, RunError>` used by both.
2. `score_name_counts`, following `experiment_score_means` (`reads/experiments.rs`):
   - more than `MAX_COUNT_NAMES` names is `InvalidRequest`; empty names return `Ok(vec![])`;
   - frame `scores` filtered by `name IN (names)`, `created_at >= from`, `created_at < to`;
   - group by `name`, plus, when `by_day`, `created_at - created_at % 86_400_000_000_000` aliased `bucket`;
   - aggregates `count(lit(1))`, `count(distinct user_id)` and `max(created_at)`;
   - sort by `name` and then `bucket`.

   Convert the batches to `ScoreNameCount`; a null `bucket` with `by_day: false` is `window.from`. The `scores` view already shows only live, non-deleted rows; check this in the view definition and, if not, add `deleted = false`.
3. Errors: `RunError::Timeout` on timeout, and DataFusion errors through `classify`/`ctx.fail`, as the neighbours do.

**Tests** (in the reads' test modules, using the existing helpers that append spans and scores):
- `error_spans_returns_only_error_spans_with_their_fields`: one trace with an `ok` and an `error` span, where the error span has `service_name`, `status_message`, `user_id`, `session_id` and one `exception` event. Exactly one row comes back, with all nine columns set.
- `error_spans_applies_the_filter`: `name = 'b'` keeps only span `b` of two error spans.
- `error_spans_caps_per_trace`: 60 error spans give 50, the earliest.
- `error_spans_excludes_eval_traces`: an error span with environment `cetra-eval` is not returned.
- `error_spans_rejects_too_many_ids` and `error_spans_with_no_ids_is_empty`.
- `score_name_counts_by_day`: three scores of `issue:a` on two days (two users, one null user) and one of `issue:b` give rows `(a, day1, 2, 2)`, `(a, day2, 1, 0)` and `(b, …)`, with the right `last_created_at`.
- `score_name_counts_whole_window` (`by_day: false`), `score_name_counts_respects_the_window`, `score_name_counts_ignores_deleted_scores`, and `score_name_counts_rejects_1001_names`.

**Done when:** `just check-rust` passes.

**Commit:** `Add error-span and score-count reads for issues`

---

### M1.3 Tables and store

**Goal:** the `meta.db` tables and their store functions.

**Edit:** new `crates/server/migrations/meta/0023_issues.sql`; new `crates/server/src/meta/issues.rs`; `crates/server/src/meta/mod.rs` (module list); `crates/server/tests/snapshots/meta_schema.snap` (accepted); `docs/spec/04-metadata-db.md` (new § Issues before § Alerts).

**Migration** (exact):
```sql
-- Track IC: issue sources, issues and their events (D-IC-1, D-IC-10).
CREATE TABLE issue_sources (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    kind          TEXT NOT NULL,
    filter        TEXT,
    enabled       INTEGER NOT NULL DEFAULT 1,
    paused_reason TEXT,
    paused_at     INTEGER,
    created_by    TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

CREATE TABLE issues (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_id   TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    fingerprint TEXT,
    title       TEXT NOT NULL,
    description TEXT,
    state       TEXT NOT NULL CHECK (state IN ('open', 'resolved', 'ignored', 'merged')),
    regressed   INTEGER NOT NULL DEFAULT 0,
    merged_into TEXT REFERENCES issues(id) ON DELETE SET NULL,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    resolved_at INTEGER,
    resolved_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX issues_source_fingerprint ON issues (source_id, fingerprint)
    WHERE fingerprint IS NOT NULL;
CREATE INDEX issues_project_last_seen ON issues (project_id, last_seen DESC, id);
CREATE INDEX issues_merged_into ON issues (merged_into) WHERE merged_into IS NOT NULL;

CREATE TABLE issue_events (
    id            TEXT PRIMARY KEY,
    issue_id      TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL CHECK (kind IN ('created', 'state_changed', 'renamed', 'merged', 'regressed')),
    actor_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    from_value    TEXT,
    to_value      TEXT,
    at            INTEGER NOT NULL
);
CREATE INDEX issue_events_issue ON issue_events (issue_id, at, id);
```
Times are ns since the epoch. `first_seen` and `last_seen` are trace start times. `actor_user_id` NULL means `system`.

**Store** (`meta/issues.rs`). Follow `meta/online_rules.rs` for style: reads take `&SqlitePool`, writes take `&mut SqliteConnection` inside `state.meta.write`, and bad stored values are `MetaError::Corrupt { table, column }`.
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind { Error }            // "error"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueState { Open, Resolved, Ignored, Merged }   // "open" … "merged"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind { Created, StateChanged, Renamed, Merged, Regressed }  // snake_case

pub(crate) struct SourceRow { id: Uuid, project_id: ProjectId, name: String, kind: SourceKind,
    filter: Option<String>, enabled: bool, paused_reason: Option<String>, paused_at: Option<i64>,
    created_by: Option<Uuid>, created_at: i64, updated_at: i64 }
pub(crate) struct IssueRow { id: Uuid, project_id: ProjectId, source_id: Uuid, fingerprint: Option<String>,
    title: String, description: Option<String>, state: IssueState, regressed: bool,
    merged_into: Option<Uuid>, first_seen: i64, last_seen: i64, resolved_at: Option<i64>,
    resolved_by: Option<Uuid>, created_at: i64, updated_at: i64 }
pub(crate) struct EventRow { id: Uuid, issue_id: Uuid, kind: EventKind, actor_user_id: Option<Uuid>,
    from_value: Option<String>, to_value: Option<String>, at: i64 }

/// An enabled error source of an active project in an active org, with what the sweeper needs.
pub(crate) struct SweepSource { id: Uuid, project_id: ProjectId, org_id: OrgId, filter: Option<String>,
    completion_timeout_s: Option<i64> }

// sources
pub(crate) async fn list_sources(pool, project) -> Result<Vec<SourceRow>, MetaError>;          // by name
pub(crate) async fn get_source(pool, project, id) -> Result<Option<SourceRow>, MetaError>;
pub(crate) async fn insert_source(conn, &SourceRow) -> Result<(), MetaError>;                  // UNIQUE name → MetaError::Conflict (as online rules)
pub(crate) async fn update_source(conn, &SourceRow) -> Result<bool, MetaError>;                // false when gone
pub(crate) async fn delete_source(conn, project, id) -> Result<bool, MetaError>;
pub(crate) async fn count_enabled_sources(conn, org) -> Result<u64, MetaError>;              // enabled, project and org active
pub(crate) async fn sweep_sources(pool) -> Result<Vec<SweepSource>, MetaError>;               // enabled AND paused_reason IS NULL, kind 'error', active project and org
// issues
pub(crate) async fn issue_by_fingerprint(conn, source, fingerprint: &str) -> Result<Option<IssueRow>, MetaError>;
pub(crate) async fn insert_issue(conn, &IssueRow) -> Result<(), MetaError>;
pub(crate) async fn get_issue(pool, project, id) -> Result<Option<IssueRow>, MetaError>;
pub(crate) async fn get_issue_tx(conn, project, id) -> Result<Option<IssueRow>, MetaError>;
pub(crate) async fn update_issue(conn, &IssueRow) -> Result<(), MetaError>;
pub(crate) async fn merged_ids(pool, project, into: Uuid) -> Result<Vec<Uuid>, MetaError>;     // issues with merged_into = into
pub(crate) async fn retarget_merged(conn, from: Uuid, to: Uuid) -> Result<u64, MetaError>;     // UPDATE … SET merged_into = to WHERE merged_into = from
pub(crate) async fn titles(pool, project, ids: &[Uuid]) -> Result<Vec<(Uuid, String, IssueState)>, MetaError>;
pub(crate) struct IssueListQuery<'a> { states: &'a [IssueState], source: Option<Uuid>, q: Option<&'a str>,
    /// When set, only these ids (at most 100).
    ids: Option<&'a [Uuid]>,
    sort: IssueSort, cursor: Option<(i64, Uuid)>, limit: u32 }
pub(crate) async fn open_counts(pool, project) -> Result<HashMap<Uuid, u64>, MetaError>;   // open issues per source
pub(crate) enum IssueSort { LastSeen, FirstSeen }   // both DESC, ties by id DESC
pub(crate) async fn list_issues(pool, project, &IssueListQuery) -> Result<Vec<IssueRow>, MetaError>; // limit + 1 rows
// events
pub(crate) async fn insert_event(conn, &EventRow) -> Result<(), MetaError>;
pub(crate) async fn list_events(pool, issue, limit: u32) -> Result<Vec<EventRow>, MetaError>;  // newest first
```
`q` matches `title LIKE '%' || ? || '%' ESCAPE '\'`, case-insensitively, with `%`, `_` and `\` escaped. Use the escaping helper that saved views or users use if one exists (grep `ESCAPE`), else a private one here.

**Tests** (unit tests in `meta/issues.rs`, with the meta test helpers the other meta modules use):
- a source round-trips; a duplicate name is a conflict;
- `count_enabled_sources` counts enabled sources of active projects only;
- `sweep_sources` leaves out disabled, paused and inactive-project sources;
- `issue_by_fingerprint` finds by (source, fingerprint), and the unique index refuses a second issue with the same fingerprint;
- `list_issues` filters by state, source, `ids` and `q` (`q = "50%"` matches the literal `50%` only), sorts by last seen with id ties, and pages with the cursor;
- `open_counts` counts open issues per source;
- `retarget_merged` moves chains;
- deleting a project cascades to sources, issues and events, and deleting a source cascades to its issues and events.

**Done when:** `just check-rust` passes after accepting the schema snapshot (`INSTA_UPDATE=always cargo test -p cetra-server --lib schema_matches_the_snapshot`, then read the diff: only the three tables and four indexes are new). Spec 04 § Issues documents the three tables (columns and meaning, as in the migration comments).

**Commit:** `Add issue tables to meta.db`

---

### M1.4 Plan limit

**Goal:** `max_active_issue_sources` is the tenth entitlement limit (D-IC-13).

**Edit:**
- `crates/server/src/entitlements/mod.rs`, `crates/server/src/config.rs`, `crates/server/src/entitlements/tokens.rs`;
- `crates/token/src/lib.rs`;
- `crates/server/src/api/entitlements.rs`, `crates/cli/src/main.rs`;
- new `crates/server/src/issues/source_limit.rs` (with a new `crates/server/src/issues/mod.rs` holding only `pub(crate) mod source_limit;`) and `crates/server/src/lib.rs` (`pub(crate) mod issues;`);
- `web/src/routes/$org/settings/plan.tsx`;
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`, `website/src/content/docs/docs/platform/plans-and-licence.mdx`;
- `fixtures/license/payloads/org-pro.json` (add the key next to `max_active_online_rules`, only if that key is there; then rerun whatever test reads the payload);
- `docs/spec/12-operations.md`, `docs/mvp-decisions.md` (D-P-13's row: append "; `max_active_issue_sources` added by D-IC-13");
- regenerated openapi and `schema.gen.ts`.

**Steps:** copy every place where `max_active_online_rules` appears, in the same order and with the same doc style, for a new `max_active_issue_sources: Option<u64>`:
1. `Limits` (doc "nine limits" → "ten limits") and `Defaults::from_config`.
2. `EntitlementDefaults` (config key `entitlements.defaults.max_active_issue_sources`, unset is unlimited) and its `Default`. Add a config test like the existing one asserting `None`.
3. `LimitsClaim`, with the same `double_option` attributes, and its test constructor.
4. `merge_limits` in `entitlements/tokens.rs`.
5. `OrgLimits`, and `From<Limits>`.
6. `crates/cli/src/main.rs` `limit_rows`: the array size goes from 9 to 10, with the row label `active issue sources`. Update any CLI test or snapshot that lists the rows.
7. `issues/source_limit.rs`, a copy of `eval/rule_limit.rs`:
   - `check(active_after: u64, limit: Option<u64>) -> Result<(), SourceLimitError>`;
   - `limit(state, org) -> Option<u64>` reading `entitlements::of(state, org).limits.max_active_issue_sources`;
   - `From<SourceLimitError> for ApiError` giving `ApiError::PlanLimit { detail }`, with the detail `"Your plan allows {n} active issue source."` for 1 and `"…sources."` otherwise;
   - its unit tests, copied from `rule_limit`'s.
8. Web `plan.tsx` `limitRows`: add `["Active issue sources", formatLimit(limits.max_active_issue_sources)]` after the online-rules row.
9. Docs: a configuration-reference row like `max_active_online_rules`'s, the plans page (Free: 1 active issue source; paid: unlimited), and spec 12's two lists of limits.
10. `just openapi`.

**Tests:** the config test (step 2), the `source_limit` tests (step 7), and `config::tests::the_configuration_reference_matches_the_loader`, which already checks the docs row.

**Done when:** `just check-rust`, `just check-web` and `just check-docs` pass.

**Commit:** `Add the active issue sources limit`

---

### M1.5 Issue writer

**Goal:** `scores::write` accepts issue occurrences: source `issue`, a client-built v7 id, no units and no cap.

**Edit:** `crates/server/src/scores.rs`.

**Steps:**
1. Add the variant `/// An issue source writing occurrences (D-IC-11). Issue { source_id: Uuid },` to `Writer`, and to every match:
   - `source`: `(Self::Issue { .. }, None | Some(ScoreSource::Issue)) => Some(ScoreSource::Issue)`;
   - `author`: `source_id`;
   - `author_user`: `None`;
   - `person`: `None`;
   - `evaluator_ids`: `(None, None)`.
2. In `write`:
   - skip `check_cap` when `writer` is `Writer::Issue { .. }` (D-IC-11: not units);
   - skip `state.usage.add` for that writer;
   - keep a comment citing D-IC-11 at both places.
3. Add:
   ```rust
   /// The id of the occurrence of `issue` on `trace` (D-IC-18): a UUIDv7 whose time is the
   /// trace's start (milliseconds, clamped at 0) and whose random bits come from SHA-256.
   pub(crate) fn occurrence_id(project: ProjectId, trace: TraceId, issue: Uuid, trace_start_ns: i64) -> Uuid
   ```
   1. `ms = max(trace_start_ns, 0) / 1_000_000` as `u64`, keeping its low 48 bits. (The caller clamps `trace_start_ns` to `now`, M1.6c.)
   2. `digest = SHA-256(b"cetra.issue-occurrence.v1" ‖ project.as_uuid().as_bytes() ‖ trace.as_bytes() ‖ issue.as_bytes())`.
   3. Bytes 0–5 are `ms` big-endian; bytes 6–15 are `digest[0..10]`.
   4. Set the version nibble: `bytes[6] = (bytes[6] & 0x0F) | 0x70`. Set the variant: `bytes[8] = (bytes[8] & 0x3F) | 0x80`.
   5. `Uuid::from_bytes(bytes)`.

   Use the project and trace id accessors that exist (`ProjectId`/`TraceId` in `cetra-core`). If one is named differently, use it and say so in the report.

**Tests:**
- `occurrence_id_is_a_v7_with_the_trace_start`: `is_v7` holds, and `v7_created_at` gives `trace_start` rounded down to the millisecond.
- `occurrence_id_is_deterministic_and_distinct` across issue, trace and project.
- `issue_writer_bills_nothing`: write two occurrences, and the usage counter is unchanged. Use the usage test helper that the existing units test uses.
- `issue_writer_ignores_the_cap`: with `set_limits(units_cap = 0)`, an issue write succeeds and an `Api` write is `402`.
- `issue_writer_rewrite_is_idempotent`: the same id twice gives one live score, and `created_at` is the trace start.

**Done when:** `just check-rust` passes.

**Commit:** `Write issue occurrences without billing them`

---

### M1.6a Tracked set

**Goal:** projects with an enabled error source are tracked and swept even without online rules, and the sweep has a hook for them.

**Edit:** new `crates/server/src/issues/sources.rs`; `crates/server/src/issues/mod.rs`; `crates/server/src/eval/trigger.rs`.

**Steps:**
1. `issues/sources.rs`:
   ```rust
   /// Error sources of one project, as the sweeper sees them.
   #[derive(Debug, Clone, PartialEq)]
   pub(crate) struct ProjectSources { pub(crate) org: OrgId, pub(crate) timeout_ns: i64, pub(crate) sources: Vec<SweepSource> }
   pub(crate) async fn refresh(state: &AppState) -> Result<HashMap<ProjectId, ProjectSources>, MetaError>
   ```
   It reads `meta::issues::sweep_sources`, groups by project in id order, and sets `timeout_ns` with `eval::trigger::timeout_ns(completion_timeout_s)` (make that fn `pub(crate)`). Add `pub(crate) mod sources;` to `issues/mod.rs`.
2. `eval/trigger.rs`:
   - Split `refresh_rules` into a private `read_rules(state) -> Result<HashMap<ProjectId, ProjectRules>, MetaError>` (everything but `set_tracked`), and `refresh_rules` = `read_rules` + `set_tracked(keys)`, unchanged for its callers and tests.
   - Add `pub(crate) async fn refresh_tracked(state) -> Result<(HashMap<ProjectId, ProjectRules>, HashMap<ProjectId, ProjectSources>), MetaError>`: `read_rules`, then `issues::sources::refresh`, then `state.pending.set_tracked(union of both key sets)`. Either error returns it before `set_tracked`, so the set stays as it was.
   - `load` and `sweep_once` call `refresh_tracked` instead of `refresh_rules`. The test faults hook `faults::take_refresh()` moves into `read_rules`, so it still fails both.
3. `sweep_once` iterates the union of project ids in ascending order:
   - `rules`: the project's rules, or an empty `Vec`;
   - `org` and `timeout_ns`: from `ProjectRules` when present, else from `ProjectSources`;
   - `sources`: the project's sources, or empty.

   `SweepCtx` gains `sources: &'a [SweepSource]`. `sweep_batch` becomes:
   1. if `ctx.sources` is not empty: `crate::issues::errors::sweep_batch(ctx, around, batch).await?` (the error sweep goes first: it is idempotent, so a later rule failure that retries the batch repeats nothing that costs a cap);
   2. if `ctx.rules` is not empty: the existing rule read and enqueue.

   In this phase `issues::errors::sweep_batch` does not exist yet. Add it in `issues/errors.rs` as `pub(crate) async fn sweep_batch(ctx: &SweepCtx<'_>, around: i64, batch: &[Completed]) -> Result<(), ()> { Ok(()) }`, with the doc comment `/// Error issues for one batch (D-IC-2). Filled in by M1.6b.`, and `pub(crate) mod errors;` in `issues/mod.rs`. It is replaced in the next phase, which is the only time a body is left empty in this plan. Make `SweepCtx` and its fields `pub(crate)`, and also `READ_TIMEOUT`, `query_options` and `Completed` if they are not, so `issues/errors.rs` can use them.
4. Tests (in `trigger.rs`'s `mod tests`, using its helpers):
   - `project_with_only_an_error_source_is_tracked`: insert an enabled error source with the meta helper (`meta::issues::insert_source` inside `state.meta.write`), run `refresh_tracked`, and `pending.observe` keeps the project's spans.
   - `a_disabled_source_untracks_its_project`.
   - `rules_only_projects_still_work`: the existing tests pass unchanged.

**Done when:** `just check-rust` passes.

**Commit:** `Track projects with error issue sources`

---

### M1.6b Fingerprints to issues

**Goal:** the error sweep reads error spans and creates, routes and regresses issues (no scores yet).

**Edit:** `crates/server/src/issues/errors.rs`, `crates/server/src/eval/trigger.rs` (tests only).

**Steps:**
1. Replace `sweep_batch`'s body in `issues/errors.rs`:
   1. Open the query context like `read_candidates` does (`ctx.state.storage.query(ctx.project, query_options(Visibility::window(ctx.state, ctx.org), Some(around)))`, then `QueryContext::new(source, None)`). On failure, warn with the project id and return `Err(())`.
   2. For each source in `ctx.sources`, in id order, call `cetra_storage::error_spans` with:
      - the source's filter;
      - the batch's ids (hex strings, as `read_candidates` builds them);
      - `READ_TIMEOUT`.

      `RunError::Invalid | RunError::InvalidRequest` skips this source; log a warning at most once per source per hour with `ctx.state.eval.filter_warning_due(source.id, ctx.now)`. Any other error returns `Err(())`.
   3. Decode the batches into
      ```rust
      struct ErrorSpan { trace_id: TraceId, span_id: SpanId, start_time: i64, service_name: Option<String>,
          name: String, exception_type: Option<String>, message: Option<String>,
          user_id: Option<String>, session_id: Option<String> }
      ```
      Use the Arrow downcasting helpers `api::convert` uses for span rows (`match_rows` is the model). A row that cannot be decoded is skipped, with a counter in the debug log.
      - Exception: the **last** element of `events` whose `name` is `exception`. Its `attributes` is JSON object text: `exception.type` and `exception.message` are its string values, and missing, non-string or unparsable means `None`.
      - `message`: the exception's message, else `status_message`.
      - Empty strings are `None`.
   4. Per span:
      - `normalized = message.as_deref().map(normalize_message)`;
      - `fp = fingerprint_hex(&ErrorFields { service_name: service_name.as_deref(), span_name: &name, exception_type: exception_type.as_deref(), message: normalized.as_deref() })`;
      - `title` = `format!("{}: {}", exception_type.as_deref().unwrap_or(&name), normalized.as_deref().unwrap_or("(no message)"))`, cut to 200 characters.

      Group by `fp`. Within a group, keep one entry per trace: its earliest span by `(start_time, span_id)`, as the occurrence span. Its user and session are those of the first span of the group's spans in that trace (same order) that has one.
   5. One `ctx.state.meta.write` per source, for each group in `fp` order:
      1. `issue_by_fingerprint(conn, source.id, &fp)`.
      2. If there is none, insert an issue:
         - `id = Uuid::now_v7()`, the title, state `open`;
         - `first_seen` and `last_seen`: the min and max occurrence time (step 6);
         - `created_at = updated_at = now`;
         - and a `created` event: no actor, `to_value = "open"`.
      3. If the issue is `merged`, follow `merged_into` once (chains are one level deep, D-IC-9). If the target is missing, keep the merged issue and log a warning with both ids.
      4. Regression: if the issue is `resolved` and any occurrence time in the group is greater than `resolved_at`, then:
         - `state = open`, `regressed = true`, `resolved_at = resolved_by = None`;
         - record a `regressed` event: no actor, `from_value = "resolved"`, `to_value = "open"`.
      5. `first_seen = min(first_seen, group min)`, `last_seen = max(last_seen, group max)`. Write with `update_issue` when anything changed, setting `updated_at = now`.
      6. Produce `Occurrence { source_id, issue_id, trace_id, span_id, at, user_id, session_id, reason: normalized }` for each trace of the group. `at = min(span start_time, ctx.now)`.
   6. Return the occurrences to the caller in this phase (`sweep_batch` keeps them in a local variable that M1.6c writes). Log one `debug!` per batch with `project_id`, `sources`, `error_spans`, `issues_created` and `occurrences`, never a title or message.
2. Tests, in `trigger.rs`'s `mod tests`, which has the span, trace and clock helpers:
   - `error_span_becomes_an_issue`: an exception `ValueError`, `bad id 42`. One issue titled `ValueError: bad id <n>`, `open`, with a `created` event.
   - `same_error_in_two_traces_is_one_issue` (the messages differ only in the number).
   - `different_exception_types_are_two_issues`.
   - `resolved_issue_regresses_on_a_newer_trace`: an older trace does not reopen it. A newer one does, with `regressed` and a `regressed` event.
   - `merged_issue_routes_to_its_target`: the target's `last_seen` moves.
   - `filter_limits_the_spans`: `service_name = 'api'`.
   - `a_bad_stored_filter_skips_the_source_only` (write the bad filter directly into the table).
   - `rules_and_sources_share_a_batch`: a rule's job is enqueued and the issue is created.
   - `eval_traces_are_never_issues`.
   - `a_failed_error_read_retries_the_batch`: use `faults::fail_next_read()` (or its equivalent in the faults module) if `error_spans` goes through the same fault hook. If it does not, make the hook in `issues/errors.rs` call `#[cfg(test)] crate::eval::trigger::faults::take_read()` the way `read_candidates` does, and make `faults` `pub(crate)` under `cfg(test)`.

**Done when:** `just check-rust` passes, including every existing `eval::trigger` test.

**Commit:** `Turn error spans into issues`

---

### M1.6c Occurrence writes

**Goal:** each (trace, issue) gets its occurrence score, guarded against deletions (D-IC-11, D-IC-18).

**Edit:** `crates/server/src/issues/errors.rs`, `crates/server/src/eval/trigger.rs` (tests only).

**Steps:**
1. Read `eval/execute.rs`'s score write path first: `write_score_unless_deleted`, `borrowed_users`, the guard check before the write, `after_commit` and `deletion::follow_up` (around lines 228–260 and 1468). Occurrences use the same sequence:
   1. Drop the occurrences that `state.deletions.guard.blocking(project, trace, &users, now)` blocks, where `users` is the occurrence's user, built as `borrowed_users` builds it.
   2. Write the rest.
   3. Run the same after-commit check, which files `deletion::follow_up` for any blocked trace that appeared meanwhile.

   If the function signatures there differ from these words, follow the code and report the difference.
2. Load the project row once per call with `meta::projects::project_by_id`. Drop occurrences whose `at` is older than `now − retention_days × DAY_NS` when `retention_days` is set: `scores::write` would refuse the whole chunk. Count them in the debug log.
3. Write the occurrences with `scores::write(state, &project_row, Writer::Issue { source_id }, items)`, one call per source, in chunks of at most 1 000. Each `ScoreCreate`:
   - `score_id: Some(occurrence_id(project, trace_id, issue_id, at))`;
   - `target: ScoreTarget::Trace { trace_id }`, plus `session_id` and `user_id` from the occurrence;
   - `name: format!("issue:{issue_id}")`, `value: ScoreValue::Boolean(true)`;
   - `reason`: the normalized message, `source: None`;
   - `metadata`: `{"cetra.issue.source_id": source_id, "cetra.issue.span_id": span hex}` (literal keys);
   - `experiment_id: None`, `dataset_item_id: None`.

   A write error logs a warning with the project id and the count, then returns `Err(())`: the batch is retried, and rewrites are idempotent.
4. Ignored issues get occurrences like the others (D-IC-10).

**Tests** (in `trigger.rs`'s `mod tests`):
- `error_span_becomes_an_occurrence`: one score `issue:<id>`, source `issue`, whose `created_at` is the span's start (ms), with the trace's user.
- `two_error_spans_of_one_issue_in_one_trace_give_one_occurrence`.
- `a_resweep_writes_no_second_occurrence`: sweep, add a later span to the same trace, sweep again: still one live score.
- `a_future_span_start_is_clamped_to_now`, and `an_occurrence_older_than_retention_is_skipped` (the rest of the batch is written).
- `ignored_issue_still_gets_occurrences`.
- `occurrences_bill_no_units`.
- `deletion_guard_blocks_occurrences`: a trace under deletion gets no score.

**Done when:** `just check-rust` passes.

**Commit:** `Write issue occurrences for error spans`

---

### M1.7 Issue sources API

**Goal:** CRUD for sources, with validation and the plan limit.

**Edit:**
- `crates/server/src/meta/issues.rs` (only if a helper is missing);
- new `crates/server/src/api/issues.rs`, `crates/server/src/api/mod.rs`;
- new `crates/server/src/routes/issues.rs`, `crates/server/src/routes/mod.rs`, `crates/server/src/http/access.rs` (registration);
- `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`;
- `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`;
- `fixtures/api/static/issue-sources.json`, `fixtures/api/static/issue-source.json`, `fixtures/api/requests/issue-source-create.json`, `fixtures/api/requests/issue-source-update.json`, `fixtures/api/README.md` (fixed ids);
- `web/src/mocks/writes.ts`;
- regenerated `openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`.

**API types** (`api/issues.rs`, `serde` + `utoipa::ToSchema`, `deny_unknown_fields` on requests):
```rust
#[serde(rename_all = "snake_case")]
pub enum IssueSourceKind { Error }

pub struct IssueSource {
    pub id: Uuid, pub project_id: Uuid, pub name: String, pub kind: IssueSourceKind,
    /// Spans filter for `error` sources.
    pub filter: Option<String>,
    pub enabled: bool,
    /// `null` when running.
    pub paused_reason: Option<String>,
    pub created_by: Option<Uuid>, pub created_at: i64, pub updated_at: i64,
    /// Issues of this source that are open now.
    pub open_issues: u64,
}
pub struct IssueSourcePage { pub data: Vec<IssueSource>, pub next_cursor: Option<String>,
    /// Enabled sources of the whole org, counted for the plan limit.
    pub org_active_sources: u64 }
pub struct CreateIssueSourceRequest { pub name: String, pub kind: IssueSourceKind,
    #[serde(default)] pub filter: Option<String>, #[serde(default = "yes")] pub enabled: bool }
pub struct UpdateIssueSourceRequest { #[serde(default)] pub name: Option<String>,
    /// Absent keeps it; `null` or blank clears it.
    #[serde(default, with = "double_option")] pub filter: Option<Option<String>>,
    #[serde(default)] pub enabled: Option<bool> }
```
Copy `OnlineRulePage`'s hand-written `PartialSchema` pattern for `IssueSourcePage`. Use the same `double_option` helper as the online-rule PATCH, or the one `LimitsClaim` uses.

**Routes** (`routes/issues.rs`, `pub(crate) fn routes() -> Vec<RouteDef>`, registered in `http/access.rs` next to online rules). All are `documented: true`. Their tag is `issues`, added to `TAGS` in `tests/openapi.rs`.

| Method | Path | Access | Operation id |
|---|---|---|---|
| GET | `/api/v1/projects/{project}/issue-sources` | `View` | `list_issue_sources` |
| POST | `…/issue-sources` | `ManageEvaluation` | `post_issue_source` |
| GET | `…/issue-sources/{id}` | `View` | `get_issue_source` |
| PATCH | `…/issue-sources/{id}` | `ManageEvaluation` | `patch_issue_source` |
| DELETE | `…/issue-sources/{id}` | `ManageEvaluation` | `delete_issue_source` |

**Steps:**
1. Validation, copied from the online rules' `validate`/`normalize_filter`:
   - `name` trimmed, 1–128 characters;
   - `filter` trimmed; blank is `None`; at most 4 096 bytes; parsed with `cetra_storage::validate_filter(&ctx, ViewName::Spans, text)` after `open_query`;
   - field errors through `field_error` and `bad_request`.
2. Create:
   - session user, as `create_online_rule` (`principal::session_user`);
   - validate, then in one `state.meta.write`: when `enabled`, `count_enabled_sources(conn, org) + 1` checked with `source_limit::check` (`403 plan_limit`);
   - `insert_source` with id `Uuid::now_v7()`; a name conflict is `409 conflict`, as for rules;
   - audit log `tracing::info!(target: "audit", source_id, project_id, user_id, "issue source created")`;
   - answer `201` with the `IssueSource`.
3. Update: enabling a disabled source runs the same limit check, inside the write. A PATCH cannot change `kind` (not in the body). Audit `"issue source updated"`.
4. Delete: `delete_source`; `404` when it does not exist. Audit `"issue source deleted"`. The cascade deletes its issues and events. The occurrence scores stay (D-IC-3).
5. List: `page_named` by name. `open_issues` comes from `open_counts` (M1.3). `org_active_sources` comes from `count_enabled_sources`.
6. OpenAPI: register the types; add `issue_source_paths`, copied from `online_rule_paths` with the five operations; add the rows to `tests/openapi.rs`.
7. Fixtures:
   - fixed ids: source `01900000-0000-7000-8000-000000000900`, issue `…-000000000901`, merged issue `…-000000000902`, events `…-000000000910` and up; add them to the README table;
   - the static JSON for list and get; request examples for create and update;
   - `api_examples.rs` entries for the two GET operations;
   - round-trips in `tests/api_examples.rs`;
   - then `just api-examples`.
8. `web/src/mocks/writes.ts`: add the three write operations to `WRITE_OPERATIONS`, and handlers like the online rules':
   - create answers `201` with the fixture merged with the body's name, kind, filter and enabled;
   - patch answers the fixture merged with the body;
   - delete answers `204`.

**Tests** (route tests in `routes/issues.rs`, copied from the online-rules route tests' setup):
- `create_list_get_update_delete_round_trip`;
- `create_rejects_a_bad_filter_with_its_position`, and `create_rejects_a_traces_only_field`: a filter on a field that only `traces` has is a 400;
- `free_plan_allows_one_enabled_source`: the second is `403 plan_limit`, and a disabled second is accepted, then enabling it is `403`;
- `viewer_can_list_but_not_create`;
- `delete_cascades_issues`;
- `duplicate_name_is_409`;
- `kind_cannot_be_patched`: a PATCH with `kind` is a 400 (unknown field).

**Done when:** `just check-rust` and `just check-web` pass (the mocks test checks every write operation).

**Commit:** `Add the issue sources API`

---

### M1.8a Issues API: reads

**Goal:** list (with an id filter), read, occurrences and events. Steps 1, 2, 3, 6 and 7 below, with their types, routes, OpenAPI, fixtures and tests.

**Edit:** `crates/server/src/api/issues.rs`, `crates/server/src/routes/issues.rs`, `crates/server/src/routes/alert_rules.rs` (only to make `encode_cursor`/`decode_cursor` `pub(crate)` if they are private), `meta/issues.rs` (only the helpers named below), `api/openapi.rs`, `tests/openapi.rs`, `api_examples.rs`, `tests/api_examples.rs`, fixtures (`static/issues.json`, `static/issue.json`, `static/issue-occurrences.json`, `static/issue-events.json`), and the regenerated files.

**Commit (8a):** `Add the issues read API`

### M1.8b Issues API: writes

**Goal:** patch and merge (steps 4, 5 and 8 below), and the removal of every M1 dead-code attribute.

**Edit:** `crates/server/src/api/issues.rs`, `crates/server/src/routes/issues.rs`, `meta/issues.rs` (only to remove its dead-code attribute), `issues/source_limit.rs`, `scores.rs` (attribute removal only), `api/openapi.rs`, `tests/openapi.rs`, fixtures (`requests/issue-update.json`, `requests/issue-merge.json`), `web/src/mocks/writes.ts`, and the regenerated files.

**Commit (8b):** `Add issue triage and merge`

The two parts share the contracts below. Their tests are split the same way: reads in 8a, writes in 8b.

**API types:**
```rust
#[serde(rename_all = "snake_case")]
pub enum IssueState { Open, Resolved, Ignored, Merged }

pub struct Issue {
    pub id: Uuid, pub project_id: Uuid, pub source_id: Uuid, pub source_name: String,
    pub kind: IssueSourceKind,
    /// Score name of its occurrences: `issue:<id>`.
    pub score_name: String,
    pub title: String, pub description: Option<String>, pub state: IssueState, pub regressed: bool,
    pub merged_into: Option<Uuid>,
    /// Issues merged into this one (their occurrences count here).
    pub merged_from: Vec<Uuid>,
    pub first_seen: i64, pub last_seen: i64,
    pub resolved_at: Option<i64>, pub resolved_by: Option<Uuid>,
    pub created_at: i64, pub updated_at: i64,
    /// Occurrences and distinct users in the requested period (list) or the last 24 h (get).
    pub count: u64, pub users: u64,
}
pub struct IssuePage { pub data: Vec<Issue>, pub next_cursor: Option<String> }
#[derive(IntoParams)]
pub struct IssueListParams {
    /// Comma-separated states; default `open`. `all` means every state but `merged`.
    #[serde(default)] pub state: Option<String>,
    #[serde(default)] pub source_id: Option<Uuid>,
    /// Substring of the title.
    #[serde(default)] pub q: Option<String>,
    /// `last_seen` (default) or `first_seen`, newest first.
    #[serde(default)] pub sort: Option<String>,
    /// Period of `count` and `users`: `[from, to)`, default the last 24 h. Same type and
    /// parsing as `ScoreListParams`'s `from`/`to` (`TimeParam`: RFC 3339 or epoch ms).
    #[serde(default)] pub from: Option<TimeParam>, #[serde(default)] pub to: Option<TimeParam>,
    /// Comma-separated issue ids, at most 100; only these are listed.
    #[serde(default)] pub ids: Option<String>,
    #[serde(default)] pub limit: Limit, #[serde(default)] pub cursor: Option<String>,
}
pub struct UpdateIssueRequest { #[serde(default)] pub title: Option<String>,
    #[serde(default, with = "double_option")] pub description: Option<Option<String>>,
    /// `open`, `resolved` or `ignored`; `merged` is refused (use merge).
    #[serde(default)] pub state: Option<IssueState> }
pub struct MergeIssueRequest { pub into: Uuid }
pub struct IssueOccurrences {
    /// One entry per UTC day in `[from, to)` with at least one occurrence.
    pub days: Vec<IssueDay>,
    /// Newest occurrences first: at most 20.
    pub examples: Vec<IssueExample>,
}
pub struct IssueDay { pub day: i64, pub count: u64, pub users: u64 }
pub struct IssueExample { pub trace_id: String, pub span_id: Option<String>, pub created_at: i64,
    pub reason: Option<String>, pub issue_id: Uuid }
pub struct IssueEvent { pub id: Uuid, pub kind: String, pub actor_user_id: Option<Uuid>,
    pub from_value: Option<String>, pub to_value: Option<String>, pub at: i64 }
pub struct IssueEventPage { pub data: Vec<IssueEvent> }
```

**Routes** (tag `issues`):

| Method | Path | Access | Operation id |
|---|---|---|---|
| GET | `/api/v1/projects/{project}/issues` | `View` | `list_issues` |
| GET | `…/issues/{id}` | `View` | `get_issue` |
| PATCH | `…/issues/{id}` | `Annotate` | `patch_issue` |
| POST | `…/issues/{id}/merge` | `Annotate` | `post_issue_merge` |
| GET | `…/issues/{id}/occurrences` | `View` | `get_issue_occurrences` (query `from`, `to`; default the last 14 days; at most 90 days) |
| GET | `…/issues/{id}/events` | `View` | `get_issue_events` (newest 100) |

`Annotate` is the member permission for triage. Check that `ProjectPerm::Annotate` is held by members and not viewers (`auth/roles.rs`); if it is not, use `ManageEvaluation` and report it.

**Steps:**
1. **Name sets.** An issue's names are `issue:<id>` plus `issue:<m>` for each `m` in `merged_ids(pool, project, id)`. For a list page, gather the names of every listed issue and its merged ones (one `merged_ids` query per issue is fine at ≤ 100 issues; or add a grouped `merged_by_target(pool, project, ids)` to `meta/issues.rs`). Call `score_name_counts` once with `by_day: false` over the period. Sum the counts back per issue. `users` is the maximum of the issue's names' user counts, documented as approximate when the issue has merged ones.
2. **List.**
   - Parse `state` (unknown is a 400 with field `state`) and `sort`.
   - Parse `ids` (each a UUID, at most 100, else a 400 with field `ids`) into `IssueListQuery.ids`.
   - Read a page with `list_issues`. The cursor is `"<sort value>:<id>"`, encoded with `encode_cursor` and read with `decode_cursor` from `routes/alert_rules.rs` (about lines 470–476; make them `pub(crate)` if they are not). A bad cursor is a 400 with field `cursor`.
   - Join the source names (`list_sources`, mapped by id), then the counts (step 1).
   - The read context comes from `open_query(state, org, project, None)`. A storage timeout is a `422 query_timeout`, as other reads.
3. **Get.** Like one list row, with counts over the last 24 h, `merged_from`, and 404 when the issue does not exist in the project.
4. **Patch**, in one meta write:
   - a `title` (trimmed, 1–200 characters) records a `renamed` event (`from_value` the old title, `to_value` the new);
   - a `description` up to 2 000 characters records no event;
   - a `state`:
     - `merged` is a 400 field `state`;
     - from `merged`, any change is a 409;
     - `resolved` sets `resolved_at = now`, `resolved_by = user` and `regressed = false`;
     - `open` or `ignored` clears `resolved_*`;
     - a change records `state_changed` with the user, from and to;
     - the same state is a no-op without an event.

   Audit log `"issue updated"`.
5. **Merge** `{into}`, in one meta write. These are refused with 409:
   - `into == id`;
   - another source;
   - `into` merged;
   - `id` already merged.

   A missing issue is a 404. Then:
   - `id.state = merged`, `merged_into = into`;
   - `retarget_merged(conn, id, into)`, so issues merged earlier into `id` now point to `into`;
   - `into.first_seen = min`, `into.last_seen = max`;
   - a `merged` event on `id` (`to_value = into`) and on `into` (`from_value = id`), both with the user.

   Answer the updated `into` issue (as `get`). Audit `"issue merged"`.
6. **Occurrences.**
   - `days`: `score_name_counts` with `by_day: true` over the issue's names, summed per day.
   - `examples`: `cetra_storage::score_list` with `source = Issue`, a name filter and newest first, limit 20, for each name of the set (at most 8 names, the issue first, then merged ones by most recent `last_seen`). Merge them and keep the 20 newest.

     `ScoreListRequest` already has `name: Option<&str>` (`reads/scores.rs:52`). Map each score to `IssueExample`, with `span_id` from the metadata key `cetra.issue.span_id` and `issue_id` parsed from the name.
7. **Events:** `list_events(pool, id, 100)`.
8. OpenAPI paths, test rows, fixtures (ids from M1.7), `api_examples` for the 4 GET operations, and the `writes.ts` handlers:
   - patch answers the issue fixture merged with the body;
   - merge answers the target fixture.

**Tests:**
- `list_defaults_to_open_and_counts_occurrences`: two occurrences written with `scores::write(Writer::Issue …)`, `count == 2`.
- `list_by_ids`, and `list_rejects_101_ids`.
- `list_filters_state_source_and_q`, and `list_pages_with_a_cursor`.
- `patch_resolve_records_an_event_and_sets_resolved_at`, `patch_reopen_clears_resolved_at`, `patch_title_records_renamed`, `patch_state_merged_is_400`, `patch_merged_issue_is_409`.
- `merge_moves_counts_to_the_target`: after a merge, the target's `count` includes the merged issue's occurrences.
- `merge_flattens_chains`: A into B, then B into C, gives A `merged_into = C`.
- `merge_refuses_other_source_self_and_merged`.
- `occurrences_days_and_examples`.
- `viewer_can_read_not_patch`.

**Done when:** `just check-rust` and `just check-web` pass.



---

### M1.9a and M1.9b Web: Issues pages

**Goal:** the Issues list (M1.9a: steps 1–4, its tests, commit `Add the Issues list page`) and the issue page (M1.9b: steps 5–6, its tests, commit `Add the issue page`) (D-IC-16). M1.9a creates `queries-issues.ts` with every query and mutation of step 2, so M1.9b only adds components and its route.

**Edit:**
- new `web/src/api/queries-issues.ts`;
- new `web/src/lib/search/issues.ts`;
- new route files `web/src/routes/$org/$project/issues/index.tsx` and `web/src/routes/$org/$project/issues/$issueId.tsx`, each with a `.test.tsx`;
- new `web/src/components/issues/` (`issue-list.tsx`, `issue-state-badge.tsx`, `issue-actions.tsx`, `merge-dialog.tsx`, `occurrence-chart.tsx`, with tests where named below);
- `web/src/components/shell/nav.ts`;
- `web/src/lib/shortcuts.ts` and `web/src/lib/shortcuts.test.ts`.

**Steps:**
1. `nav.ts`: add `{ section: "Monitor", to: "/$org/$project/issues", label: "Issues", icon: Bug }` after Alerts (lucide `Bug`). In `shortcuts.ts` `GO_TARGETS`, add `y` → Issues, and add its row to `SHORTCUTS` (the help dialog list) next to the other `g` rows. The sidebar tooltip follows from `goKeyOf`.
2. Queries, following `queries-online-rules.ts`:
   - `issuesInfiniteQuery(org, project, search)` on `GET …/issues` with `state`, `source_id`, `q`, `sort`, and `from`/`to` = the last 24 h, fixed when the page opens;
   - `issueQuery`, `issueOccurrencesQuery` (last 14 days), `issueEventsQuery`, and `issueSourcesQuery` (for the source filter);
   - mutations `usePatchIssue` and `useMergeIssue`, which invalidate the issue, the list and the events.
3. Search params (`lib/search/issues.ts`, like `lib/search/online-rules.ts`): `state` (`open` default, `resolved`, `ignored`, `all`), `source` (id), `q`, `sort` (`last_seen` default, `first_seen`).
4. List page (`issues/index.tsx`):
   - state tabs Open, Resolved, Ignored and All; a source select ("All sources" and each source); a search box (debounced 300 ms); a sort select;
   - a table with these columns:
     - title, with a "Regressed" badge when `regressed`;
     - source name;
     - state badge;
     - count and users in the last 24 h;
     - last seen and first seen, as relative times with the existing time component;
   - rows link to the issue page, and `j/k` move as in other lists;
   - empty state: "No issues yet. Add an error source to start finding issues.", with a link to Issue sources when the user can manage evaluation, else without the link;
   - loading and error states as on the online-rules list.
5. Issue page (`issues/$issueId.tsx`):
   - breadcrumb item: the title;
   - header: an editable title (inline edit, Enter saves, Esc cancels; only with `Annotate`), the state badge, the regressed badge, and the source name;
   - actions (`issue-actions.tsx`):
     - Resolve, Ignore or Reopen, depending on the state;
     - "Merge into…" (`merge-dialog.tsx`: a searchable list of open issues of the same source, excluding itself; confirm, then navigate to the target);
     - "View traces", which navigates to `/$org/$project/traces` with the filter `has_score('issue:<id>') OR has_score('issue:<m1>') …`. The names are the issue then its `merged_from`, at most 8 in all. When `merged_from` has more than 7, a note says "Showing traces of the 8 issues with the most recent occurrences." For this the page sorts `merged_from` with its own `get_issue` calls; if that is too costly, it uses the API order and the note says "8 of N merged issues".
   - a merged issue shows a banner "Merged into <title>" with a link, and no actions;
   - body:
     - the description (editable);
     - `occurrence-chart.tsx`: bars per day over 14 days from `days`, with missing days as zero, using the chart component the dashboard bar widget uses;
     - stats: occurrences in 14 days, users, first seen and last seen;
     - "Examples": the newest 20, each linking to the trace (and span), with its reason as plain text;
     - "History": the events (created, state changes with the actor's name from the members query used elsewhere, else "System", renamed, merged, regressed).
6. Copy the online-rules page tests' patterns (`renderApp`, `server.use`, fixtures from `fixtures/api/static`).

**Tests:**
- list: renders fixture rows; switching the tab changes `state` in the request; the source filter and search update the URL; the empty state with and without manage permission.
- issue page: renders the title, chart and examples; Resolve sends `PATCH {state: "resolved"}`; a viewer sees no actions; merge dialog lists same-source issues and posts `{into}`; "View traces" navigates with the expected filter text; the merged banner.
- shortcuts: `g y` goes to Issues.

**Done when:** `just check-web` passes.

**Commits:** M1.9a `Add the Issues list page`; M1.9b `Add the issue page`.

---

### M1.10 Web: Issue sources pages

**Goal:** list, create, edit, enable, disable and delete error sources.

**Edit:**
- new `web/src/routes/$org/$project/issue-sources/index.tsx`, `new.tsx`, `$sourceId/edit.tsx` (each with a `.test.tsx`);
- new `web/src/components/issue-sources/` (`source-form.tsx`, `source-list.tsx`);
- `web/src/api/queries-issues.ts` (source mutations);
- `web/src/components/shell/nav.ts`.

**Steps:**
1. `nav.ts`: `{ section: "Evaluate", to: "/$org/$project/issue-sources", label: "Issue sources", icon: Radar }`, after Online rules (lucide `Radar`). No shortcut.
2. List:
   - rows with name, kind ("Errors"), filter (monospace, truncated), Enabled switch (with `ManageEvaluation`), open issues (linking to Issues filtered by the source), and updated;
   - a "New source" button;
   - a `plan_limit` answer from create or enable shows its detail and the upgrade line, as on the online-rules list;
   - when `org_active_sources` has reached the plan limit, "New source" stays enabled, and the server answers.
3. Form (`source-form.tsx`), for new and edit:
   - name;
   - kind: one option, "Errors", shown read-only on edit;
   - filter: the filter editor component the online-rule form uses for span rules, set to the spans view, with the help text "Error spans that match this filter become issues. `status = 'error'` is always added.";
   - enabled.

   Server field errors show under their fields, using the online-rule form's error mapping.
4. Delete, from the edit page, with a confirm dialog: "Delete this source and its N issues? Their occurrence scores stay on the traces." N is `open_issues`, and the text says "open issues" to be exact.

**Tests:** list renders; toggle sends PATCH; create posts and navigates back; a 400 field error shows under the filter; a `plan_limit` shows the upgrade line; a viewer sees no New button.

**Done when:** `just check-web` passes.

**Commit:** `Add the Issue sources pages`

---

### M1.11 Web: trace chips and the issue filter

**Goal:** a trace shows its issues, and the trace list can filter by issue.

**Edit:** `web/src/components/trace/trace-header.tsx` (or the component under it that holds header chips: put the chips row there), new `web/src/components/issues/issue-chips.tsx` (+ test), `web/src/api/queries-issues.ts`, `web/src/components/facets/preset-chips.tsx` (+ its test).

**Steps:**
1. `traceIssuesQuery(org, project, traceId)`: `GET …/scores?trace_id=<id>&source=issue`, using the same scores query parameters `scoresQuery` uses, plus `source`, with limit 100. The issue ids come from the names (`issue:` prefix), and their titles and states from one `GET …/issues?state=all&ids=<ids>` (M1.8a). A merged issue's chip links to the issue page, which shows the merge banner.
2. `issue-chips.tsx`: one chip per issue (state colour: open red, resolved green, ignored grey), with the title truncated to 40 characters and a link to the issue page. It renders nothing when there are no issues. It sits in the trace header's chip row.
3. `preset-chips.tsx`: a "Has issue" menu next to "Has score". It lists the 20 most recent open issues (`GET …/issues?state=open&limit=20`), and picking one appends `has_score('issue:<id>')` to the filter, the same way "Has score" appends its call.

**Tests:** chips render from mocked scores and issues, and link; no chips without issue scores; the "Has issue" menu adds the filter text.

**Done when:** `just check-web` passes.

**Commit:** `Show issues on traces and filter traces by issue`

---

### M1.12 Docs and closeout

**Goal:** the specs, the user docs, the changelog and the roadmap describe M1.

**Edit:**
- `docs/spec/06-evaluation.md`: new § Issues (sources, error fingerprints, lifecycle, occurrences, limits), summarizing D-IC-1, 2, 3, 9, 10, 11, 13 and 18 for the error kind;
- `docs/spec/09-api.md`: the issue-sources and issues routes, in a new § Issues;
- `docs/spec/11-ui.md`: § Navigation (the Monitor section gets Issues, `g y`; the Evaluate section gets Issue sources), and new § Issues;
- new `website/src/content/docs/docs/evaluation/issues.mdx`, with its sidebar entry in `website/astro.config.mjs` after Online rules:
  - what issues are;
  - creating an error source;
  - how errors are grouped (the fingerprint fields and placeholders, in plain words);
  - states and regression; merging;
  - "View traces"; the Free plan limit.
  Follow `website/STYLE.md`. Code blocks are marked untested unless tested.
- `CHANGELOG.md` `### Added`: `- Issues: error spans of production traces are grouped into issues you can resolve, ignore and merge, with occurrence counts and a filter on the trace list (D-IC-2).`
- `docs/roadmap.md` IC row: Status "**M1 implemented** (error issues). M2 implemented (provider calls). M3–M5 to do."

**Done when:** `just check` passes.

**Commit:** `Document error issues`

---

## Closing

After M1.12, a Sonnet review reads the M1 diff against this plan and the design. Its findings are fixed by a Haiku agent with an exact brief, then checked.
