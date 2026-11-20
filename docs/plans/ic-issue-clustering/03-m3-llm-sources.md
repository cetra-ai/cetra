# IC M3 · LLM issue sources — plan

M3 adds the three LLM kinds of issue source (`score`, `feedback`, `topic`, D-IC-1). Each candidate gets an LLM summary and an embedding (D-IC-4, D-IC-5), joins the nearest issue or waits for a refit (D-IC-7). A refit every 6 h, or on demand, turns the waiting candidates into new named issues (D-IC-8). The work runs under budgets, daily caps, pauses and retries (D-IC-12). Pruning and the deletion cascade cover the new data (D-IC-6). The API and the source editor gain the new kinds.

**Prerequisites:** M1 (error issues) and M2 (provider calls) are committed on the branch this one is cut from.

**Read first:**
- `AGENTS.md`;
- `00-design.md` (D-IC-1, D-IC-4 to D-IC-8, D-IC-11, D-IC-12, D-IC-17, D-IC-18);
- M1's and M2's plans (`01-…`, `02-…`), for the code they added;
- track I's code: `crates/server/src/eval/{mod,dispatch,execute,budget,retry,sampling,trigger,trace_load,connection}.rs`, `meta/eval_jobs.rs`, `meta/online_rules.rs` (pauses), `meta/rule_spend.rs`;
- `routes/connections.rs` (`test_connection`, `recover_connection`, the key-update route), `meta/evaluators.rs` (`connection_in_use`);
- `deletion/resolve.rs`;
- `crates/server/src/issues/{errors,sources,source_limit}.rs`, `meta/issues.rs`, `routes/issues.rs`, `api/issues.rs`;
- `crates/eval/src/runner.rs` (`complete`, `embed`), `crates/eval/src/issue/cluster.rs`;
- `web/src/lib/score-tone.ts` (`goodness`, `toneOf`): the bad-third rule ported in M3.3.

## What it is, and what it is not

When M3 is done, a member can create a `score`, `feedback` or `topic` source with a chat connection and model, an embedding connection and model, a sampling rate, a daily cap, an optional budget, a floor and a minimum issue size. Then:
- bad scores, and new production traces, become candidates;
- the IC workers summarize and embed them, on the same per-connection limits as judges;
- candidates join existing issues online;
- every 6 h (or on "Refit now") the waiting ones are clustered into new named issues;
- the source page shows its refits; budgets, pauses and the connection test work for sources as for rules;
- deleted traces and users take their candidates with them.

Out of scope:
- alerts, notifications, datasets and MCP (M4);
- annotation queues (M5);
- backfill (a non-goal).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IC-1 | Source fields; sampling and the daily cap for LLM kinds only |
| D-IC-4 | One summary, then one embedding; prompts in code; the triggers; session-target input |
| D-IC-5 | Embedding connection, model and dimension fixed per source; Anthropic cannot embed; 512 dimensions requested |
| D-IC-6 | Vectors in `meta.db`; retention of vectors and candidates; pruning to the data window |
| D-IC-7 | Join at cosine ≥ floor; running mean up to 500 members |
| D-IC-8 | Refit: ≥ 20 pending of 7 days, ≤ 5 000; average linkage; ≥ `min_issue_size`; join or name; outliers after 7 days |
| D-IC-11, D-IC-18 | Occurrence scores (M1's writer and ids) |
| D-IC-12 | Own queue with I's job columns; IC worker pool; I's per-connection limiter; retries, pauses, budget; refit history |
| D-IC-17 | Call-trace shape |
| D-I-6, D-I-10, D-I-12–D-I-16 | Limiter, sampling, job states, retries, pauses, budget, call trace per attempt |
| D-I-47 | Score direction and the bad third |

**Decided while writing this plan** (M3.1 adds them to `00-design.md` and the decision log as D-IC-19):
- **Prompt inputs.** The summary prompt uses the root span's full `input` and `output`, rendered to text and cut to 4 000 characters each (the `*_preview` columns are only 512). The topic prompt uses the **first user message**:
  - when the root input is in `messages` format, the text parts of its first `role: "user"` message, joined with a newline;
  - otherwise the root input text;
  - cut to 4 000 characters.
- **Summary kept across retries.** A candidate's summary is stored as soon as it is made, so a failed embedding retries without a second summary call.
- **Paused detail.** `issue_sources.paused_detail` holds the provider's error text for a connection pause, shown on the source page like a rule's.
- **Refit schedule.** `issue_sources.next_refit_at` (ns): creation + 1 h, then the end of each refit + 6 h. "Refit now" sets it to now, and is refused with 409 when a refit started less than 10 minutes ago.
- **Naming waits for a slot.** A refit takes a chat-connection slot for each naming call, retrying every second for up to 2 minutes. If none comes, the refit stops with error class `busy`, and the issues it already created stay.
- **Embedding connection test.** `POST …/llm-connections/{id}/test` takes an optional `embedding_model`. When it is set, the test embeds `"ping"` instead of asking for a judgement. A success resumes the sources paused for that connection.

## Versions

No new dependency.

## Amendments (cold review; binding, they override the phase text where they differ)

Each phase lists the amendments it must apply.

- **A1 Struct literals.** New fields on `SourceRow` and `IssueRow` (M3.1a) break every literal outside `meta/issues.rs`. M3.1a fixes them all:
  - find them with `grep -rn "SourceRow {\|IssueRow {" crates/server/src` (today: `routes/issues.rs` ~249, ~305, ~1490, ~1577, ~1902; `eval/trigger.rs` ~1030, ~2067; `issues/errors.rs` ~657; the conversions in `api/issues.rs`);
  - new `Option` fields are `None`, list fields `Vec::new()`, and `member_count` is `0`;
  - an update that rebuilds a row from the stored one copies the stored values.
  Those files are in M3.1a's Edit list.
- **A2 Visibility.** These become `pub(crate)` in the phase that first uses them, and that phase lists the file:
  - in `eval/execute.rs`: `connection_unchanged`, `retry_write`, `spent` and `commit_unless_deleted` (`commit_judge_trace`, `count_judge_usage` and `after_commit` already are);
  - in `issues/errors.rs`: `write_occurrences`, `Occurrence` and its fields, and `occurrence_score`.
- **A3 Occurrence span.** `Occurrence.span_id` becomes `Option<SpanId>`. `occurrence_score` writes the `cetra.issue.span_id` metadata key only when it is `Some`.
  - Error issues keep their span.
  - A candidate's occurrence has the candidate's `span_id` (a span-target score), else `None`.
  - Its trace is the candidate's resolved `trace_id` (A4), and `at = min(trace_start, now)`.
- **A4 Resolved target.** Add `pub(crate) async fn set_candidate_target(conn, id: Uuid, claims: u32, trace_id: TraceId, trace_start: i64, user_id: Option<&str>, session_id: Option<&str>, now: i64) -> Result<bool, MetaError>`, fenced by `status = 'running' AND claims = ?` (M3.1b).
  - The worker calls it right after a successful load, so retries, refits and the deletion cascade see the trace.
  - A session target resolves to a trace: read the score (`cetra_storage::score_list` by `score_id`) for its `created_at`; then the newest trace of the session that started before it, with the read `routes/sessions.rs` uses to list a session's traces (filter `session_id = '<id>'`, the window `[created_at − 30 days, created_at + 1 ns)`, newest first, limit 1). If there is none: `Dropped`.
  - A user or session already set on the candidate is kept.
- **A5 Finish variants.** `CandidateFinish` is exactly:
  - `Summarized { summary, prompt_version, call_trace_id }`: stays `running`.
  - `Pending { vector, embed_model, dimension }`.
  - `Assigned { issue_id, vector, embed_model, dimension }`.
  - `Dropped`.
  - `Retry { not_before }` → `queued`.
  - `LoadRetry { not_before }` → `queued`, `load_retries + 1`.
  - `Requeue { not_before }` → `queued`, no counter changes (used before any attempt: source missing or paused, trace load saturated, no connection slot).
  - `PauseRequeue { not_before }` → `queued`, `attempts − 1` (only after `start_candidate_attempt`).
  - `Failed { class }`.
  - `Skipped { class }`.

  `finish_candidate(conn, id, claims, finish, spent_usd: f64, now)` adds `spent_usd` to the row's `cost_usd` for every variant. The caller runs it and `add_spend(conn, source, day_start(now), spent_usd)` (when > 0) in one `state.meta.write` closure.
  - A load failure with `load_retries` already 3 is `Failed { class: "trace_not_found" }` (three retries, then the fourth failure fails).
  - A fence that returns `false` stops the candidate silently, with nothing more done.
  - Summary and embedding calls share the 6 attempts and the backoff; this is intended.
- **A6 Deletion guard on calls.** The worker follows `execute.rs`:
  - check 0 at the start;
  - check A after the load, with the trace's users;
  - each call trace committed through `commit_unless_deleted`, then `after_commit` for the follow-up;
  - a block at any point gives `Skipped { class: "deleted" }`.
  The refit's naming traces hold only summaries and are not guarded (accepted).
- **A7 Queued expiry.** `prune_candidates` also sets `Skipped { class: "expired" }` on candidates still `queued` more than 1 day after `created_at` (a paused source's backlog).
- **A8 Refit schedule.**
  - Every refit, success or error (`busy`, `budget`, `cluster` or a naming error), ends with `next_refit_at = finished_at + 6 h`.
  - `eval::recover` closes refit rows with `finished_at IS NULL` (`finished_at = now`, `error_class = 'interrupted'`), through `close_open_refits(conn, now)` in M3.1b.
  - The refit route answers 409 when a refit started less than 10 minutes ago **or** when `next_refit_at <= now` (one is already due).
  - One refit can hold the loop for a long time (naming waits up to 2 minutes per cluster). Other sources wait; this is accepted.
- **A9 Refit writes.** Each join and each new issue is one `state.meta.write` that reads `source_centroids` or `get_issue_tx` inside it, then writes the issue and runs `set_candidates_assigned(conn, ids, issue, now) -> Result<Vec<Uuid>, MetaError>`, fenced `AND status = 'pending'`. It returns the ids actually assigned, and occurrences are written only for those. The pending list and vectors are read outside, so a candidate deleted or assigned in between is skipped by the fence.
- **A10 Determinism and ids.**
  - `pending_candidates` orders by `created_at DESC, id DESC`, and the refit re-sorts the kept list by `(created_at, id)` ascending.
  - `call_trace_id(owner: Uuid, claims: u32, call: u32)` uses the call as `u32` BE: 1 summarize, 2 embed, `3 + cluster index` naming.
  - A refit uses `claims = 0`.
- **A11 Triggers wiring.** `ProjectSources` gains `topics: Vec<SourceRow>`, and `issues::sources::refresh` fills it from `llm_sources`; a project with only topic sources takes its `org` and `timeout_ns` from that tuple. A separate `pub(crate) async fn refresh_score_rules(state) -> Result<(), MetaError>` in `triggers.rs` rebuilds the score rules:
  - `refresh` calls it;
  - so do the source routes after every create, update and delete (moved from M3.7 to M3.3; `routes/issues.rs` is in M3.3's Edit list for these three calls only).

  `on_scores_written` only logs its errors. A burst of bad scores makes one meta write per `scores::write` call, which is accepted. Recursion is impossible: occurrence writes use `Writer::Issue`, which returns first.
- **A12 Panics and reservations.** `run_candidate` runs inside `AssertUnwindSafe(…).catch_unwind()`, as `dispatch::run_claimed` does; a panic finishes the candidate `Failed { class: "internal" }`. A budget `Reservation` is settled on every success path and released by `Drop` otherwise.

## Phases

| Phase | Edits (summary) | Depends on |
|---|---|---|
| M3.1a Tables and rows | migration `0024`, `SourceRow`/`IssueRow` fields, vector codec, every struct literal (A1), spec 04, design doc, decision log | — |
| M3.1b Store functions | `meta/issues.rs` candidate, spend, refit and source functions (A4, A5, A8, A9, A10) | M3.1a |
| M3.2 Limiter and config | `eval/dispatch.rs`, `config.rs`, configuration reference | — |
| M3.3 Triggers | new `issues/bad_score.rs`, new `issues/topics.rs`, new `issues/triggers.rs`, `scores.rs`, `eval/trigger.rs`, `issues/sources.rs`, `http/mod.rs`, `serve.rs` | M3.1 |
| M3.4a Prompts and shared code | new `issues/prompts.rs`, `issues/errors.rs` (join, occurrences: A2, A3), `issues/mod.rs`, `eval/execute.rs` (A2) | M3.1b |
| M3.4b Worker loop | new `issues/worker.rs` (loop, `call_trace_id`), `http/mod.rs`, `serve.rs`, `eval/mod.rs` (spawn, recover, janitor) | M3.4a |
| M3.4c Candidate run | `issues/worker.rs` (`run_candidate`) | M3.4b |
| M3.5 Refit | new `issues/refit.rs`, `issues/prompts.rs`, `issues/mod.rs`, `eval/mod.rs` | M3.4 |
| M3.6a Pauses, budget and pruning | `meta/issues.rs`, `issues/worker.rs`, `issues/refit.rs`, `eval/dispatch.rs`, `eval/execute.rs` (A2) | M3.5 |
| M3.6b Connection test, connection guard, deletion | `routes/connections.rs`, `api/connections.rs`, `meta/evaluators.rs`, `deletion/resolve.rs`, spec 03 and 09 | M3.6a |
| M3.7 API | `api/issues.rs`, `routes/issues.rs`, openapi, fixtures, mocks | M3.1, M3.5 |
| M3.8 Web and docs | source editor and page, issues list kind filter, docs page, CHANGELOG, specs, roadmap | M3.7 |

All phases run one after the other in one worktree.

**Rules for every phase** (repeat them in each brief):
- Follow the plan exactly. If a step is unclear, contradicts the code or cannot be done as written, stop and report instead of improvising.
- Touch only the phase's **Edit** files.
- **No compiling or testing during the phases** (the user's rule: the machine is overloaded). The only command allowed is `cargo fmt --all`. Write each test with its phase. Every check runs once, when all of M3 is committed.
- Do not regenerate generated files (`openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`, insta snapshots). They are regenerated at the end of the milestone.
- No `unwrap()`/`expect()` outside tests unless a comment states the invariant. No payload text (summaries, titles, prompts, vectors, provider messages) in logs; log ids, counts and error classes. Never log or store the provider message of an embedding error: it can echo the input.
- **Dead code:** a new `pub(crate)` item with no non-test caller yet gets `#[cfg_attr(not(test), allow(dead_code))]`. M3.7 removes every attribute M3 added.
- Commit once, with the given subject, staging only the phase's files (`git add <paths>`).

---

### M3.1 Tables and store (M3.1a, then M3.1b)

**Goal:** the columns and tables of the LLM kinds, and their store functions.

**Split:**
- **M3.1a**, commit `Add issue candidate, spend and refit tables`: the migration; `SourceKind`'s new variants; the new fields of `SourceRow` and `IssueRow` with their reads and writes in the existing functions; `encode_vector` and `decode_vector`; every struct literal (A1); the spec, design doc and decision log edits; the round-trip and codec tests.
- **M3.1b**, commit `Add issue candidate and refit store functions`: every function of store-additions item 4, with A4 (`set_candidate_target`), A5 (`finish_candidate`'s variants and `spent_usd`), A8 (`close_open_refits`), A9 (`set_candidates_assigned` returning the assigned ids, fenced on `pending`) and A10 (ordering), and their tests.

**Edit:**
- new `crates/server/migrations/meta/0024_issue_candidates.sql`;
- `crates/server/src/meta/issues.rs`;
- `docs/spec/04-metadata-db.md` (§ Issues);
- `docs/plans/ic-issue-clustering/00-design.md` (a new D-IC-19 with this plan's "Decided while writing this plan");
- `docs/mvp-decisions.md` (the D-IC-19 row, one paragraph summarizing the same six points).

**Migration** (exact):
```sql
-- Track IC M3: LLM issue sources, candidates, spend and refits (D-IC-1, D-IC-6, D-IC-12, D-IC-19).
ALTER TABLE issue_sources ADD COLUMN sample_rate REAL;
ALTER TABLE issue_sources ADD COLUMN daily_cap INTEGER;
ALTER TABLE issue_sources ADD COLUMN score_names TEXT;
ALTER TABLE issue_sources ADD COLUMN feedback_sources TEXT;
ALTER TABLE issue_sources ADD COLUMN chat_connection_id TEXT REFERENCES llm_connections(id);
ALTER TABLE issue_sources ADD COLUMN chat_model TEXT;
ALTER TABLE issue_sources ADD COLUMN embed_connection_id TEXT REFERENCES llm_connections(id);
ALTER TABLE issue_sources ADD COLUMN embed_model TEXT;
ALTER TABLE issue_sources ADD COLUMN embed_dimension INTEGER;
ALTER TABLE issue_sources ADD COLUMN floor REAL;
ALTER TABLE issue_sources ADD COLUMN min_issue_size INTEGER;
ALTER TABLE issue_sources ADD COLUMN budget_usd REAL;
ALTER TABLE issue_sources ADD COLUMN budget_period TEXT;
ALTER TABLE issue_sources ADD COLUMN paused_detail TEXT;
ALTER TABLE issue_sources ADD COLUMN next_refit_at INTEGER;

ALTER TABLE issues ADD COLUMN centroid BLOB;
ALTER TABLE issues ADD COLUMN member_count INTEGER NOT NULL DEFAULT 0;

CREATE TABLE issue_candidates (
    id                     TEXT PRIMARY KEY,
    project_id             TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_id              TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    target                 TEXT NOT NULL CHECK (target IN ('trace', 'span', 'session')),
    trace_id               BLOB,
    span_id                BLOB,
    session_id             TEXT,
    user_id                TEXT,
    trace_start            INTEGER,
    score_id               TEXT,
    dedup_key              TEXT NOT NULL UNIQUE,
    status                 TEXT NOT NULL CHECK (status IN ('queued', 'running', 'pending', 'assigned', 'outlier', 'dropped', 'failed', 'skipped')),
    attempts               INTEGER NOT NULL DEFAULT 0,
    claims                 INTEGER NOT NULL DEFAULT 0,
    interruptions          INTEGER NOT NULL DEFAULT 0,
    load_retries           INTEGER NOT NULL DEFAULT 0,
    not_before             INTEGER NOT NULL,
    error_class            TEXT,
    issue_id               TEXT REFERENCES issues(id) ON DELETE SET NULL,
    summary                TEXT,
    summary_prompt_version INTEGER,
    vector                 BLOB,
    embed_model            TEXT,
    dimension              INTEGER,
    call_trace_id          BLOB,
    cost_usd               REAL NOT NULL DEFAULT 0,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL
);
CREATE INDEX issue_candidates_ready ON issue_candidates (not_before) WHERE status = 'queued';
CREATE INDEX issue_candidates_source ON issue_candidates (source_id, status, created_at);
CREATE INDEX issue_candidates_trace ON issue_candidates (project_id, trace_id);

CREATE TABLE issue_source_days (
    source_id  TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    day        INTEGER NOT NULL,
    candidates INTEGER NOT NULL DEFAULT 0,
    spent_usd  REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (source_id, day)
);

CREATE TABLE issue_refits (
    id          TEXT PRIMARY KEY,
    source_id   TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    trigger     TEXT NOT NULL CHECK (trigger IN ('schedule', 'manual')),
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    pending     INTEGER NOT NULL DEFAULT 0,
    clusters    INTEGER NOT NULL DEFAULT 0,
    created     INTEGER NOT NULL DEFAULT 0,
    joined      INTEGER NOT NULL DEFAULT 0,
    outliers    INTEGER NOT NULL DEFAULT 0,
    cost_usd    REAL NOT NULL DEFAULT 0,
    error_class TEXT
);
CREATE INDEX issue_refits_source ON issue_refits (source_id, started_at DESC);
```

Column meanings:
- `score_names` and `feedback_sources` are JSON arrays of strings.
- `day` is the UTC day start in ns.
- `dedup_key` is `<source id>:<trace hex>` for a trace target, `<source id>:<trace hex>:<span hex>` for a span, and `<source id>:session:<session id>` for a session.
- A vector is f32 little-endian.

**Store additions** (`meta/issues.rs`):
1. `SourceKind` gains `Score` (`"score"`), `Feedback` (`"feedback"`) and `Topic` (`"topic"`), with `is_llm(self) -> bool` (true for those three).
2. `SourceRow` gains the new columns as typed fields:
   ```rust
   sample_rate: Option<f64>, daily_cap: Option<u32>, score_names: Vec<String>, feedback_sources: Vec<String>,
   chat_connection_id: Option<Uuid>, chat_model: Option<String>, embed_connection_id: Option<Uuid>,
   embed_model: Option<String>, embed_dimension: Option<u32>, floor: Option<f64>, min_issue_size: Option<u32>,
   budget_usd: Option<f64>, budget_period: Option<String>, paused_detail: Option<String>, next_refit_at: Option<i64>
   ```
   An empty `Vec` is stored as NULL, and NULL reads back as an empty `Vec`. Every read and write of `issue_sources` carries them, and `update_source` writes them all except `kind`, `project_id`, `embed_connection_id`, `embed_model` and `embed_dimension`; those three embedding fields change only through `set_embed_dimension`.
3. `IssueRow` gains `centroid: Option<Vec<f32>>` and `member_count: u64`. `insert_issue` writes them. `update_issue` writes `member_count` and `centroid` too. Add `fn encode_vector(&[f32]) -> Vec<u8>` and `fn decode_vector(&[u8]) -> Result<Vec<f32>, MetaError>` (a length that is not a multiple of 4 is `Corrupt`). Both are `pub(crate)`.
4. New types and functions:
   ```rust
   pub(crate) enum CandidateStatus { Queued, Running, Pending, Assigned, Outlier, Dropped, Failed, Skipped } // snake_case
   pub(crate) enum CandidateTarget { Trace, Span, Session }
   pub(crate) struct CandidateRow { id: Uuid, project_id: ProjectId, source_id: Uuid, target: CandidateTarget,
       trace_id: Option<TraceId>, span_id: Option<SpanId>, session_id: Option<String>, user_id: Option<String>,
       trace_start: Option<i64>, score_id: Option<Uuid>, dedup_key: String, status: CandidateStatus,
       attempts: u32, claims: u32, interruptions: u32, load_retries: u32, not_before: i64,
       error_class: Option<String>, issue_id: Option<Uuid>, summary: Option<String>,
       summary_prompt_version: Option<u32>, vector: Option<Vec<f32>>, embed_model: Option<String>,
       dimension: Option<u32>, call_trace_id: Option<TraceId>, cost_usd: f64, created_at: i64, updated_at: i64 }
   /// Inserts unless `dedup_key` exists; true when inserted.
   pub(crate) async fn insert_candidate(conn, &CandidateRow) -> Result<bool, MetaError>;
   /// Adds one to today's count when it is below `cap`; false at the cap.
   pub(crate) async fn admit_daily(conn, source: Uuid, day: i64, cap: u32) -> Result<bool, MetaError>;
   pub(crate) async fn add_spend(conn, source: Uuid, day: i64, usd: f64) -> Result<(), MetaError>;
   pub(crate) async fn spent_between(pool, source: Uuid, from_day: i64, to_day: i64) -> Result<f64, MetaError>;
   /// Queued candidates ready at `now` of enabled, unpaused sources of active projects and orgs, oldest
   /// `not_before` first, whose chat and embedding connections are not in `saturated`.
   pub(crate) async fn ready_candidates(pool, now: i64, saturated: &[Uuid], limit: u32) -> Result<Vec<CandidateRow>, MetaError>;
   /// `queued → running`, `claims += 1`; the new claims, or None when no longer queued.
   pub(crate) async fn claim_candidate(conn, id: Uuid, now: i64) -> Result<Option<u32>, MetaError>;
   /// `attempts += 1` fenced by `claims`; the new attempts, or None.
   pub(crate) async fn start_candidate_attempt(conn, id: Uuid, claims: u32, now: i64) -> Result<Option<u32>, MetaError>;
   pub(crate) enum CandidateFinish {
       Summarized { summary: String, prompt_version: u32, cost_usd: f64, call_trace_id: TraceId }, // stays running
       Pending { vector: Vec<f32>, embed_model: String, dimension: u32, cost_usd: f64 },
       Assigned { issue_id: Uuid, vector: Vec<f32>, embed_model: String, dimension: u32, cost_usd: f64 },
       Dropped { cost_usd: f64 },
       Retry { not_before: i64 },          // back to queued
       Requeue { not_before: i64 },        // back to queued, attempts − 1 (saturated or paused)
       Failed { class: &'static str },
       Skipped { class: &'static str },    // budget, deleted
   }
   /// Applies a finish fenced by `status = 'running' AND claims = ?`; false when the fence fails.
   pub(crate) async fn finish_candidate(conn, id: Uuid, claims: u32, finish: CandidateFinish, now: i64) -> Result<bool, MetaError>;
   /// Startup and janitor (D-I-12): running rows not updated since `older_than` go back to queued with
   /// `interruptions + 1`; at 3 interruptions they fail with class `interrupted`.
   pub(crate) async fn interrupt_candidates(conn, older_than: i64, now: i64) -> Result<u64, MetaError>;
   /// Non-merged issues of a source with a centroid: (id, state, centroid, member_count).
   pub(crate) async fn source_centroids(conn, source: Uuid) -> Result<Vec<(Uuid, IssueState, Vec<f32>, u64)>, MetaError>;
   /// Pending candidates of a source created at or after `since`, newest first, at most `limit`.
   pub(crate) async fn pending_candidates(conn, source: Uuid, since: i64, limit: u32) -> Result<Vec<CandidateRow>, MetaError>;
   pub(crate) async fn set_candidates_assigned(conn, ids: &[Uuid], issue: Uuid, now: i64) -> Result<(), MetaError>;
   /// Pending candidates created before `before` become outliers; returns how many.
   pub(crate) async fn mark_outliers(conn, source: Uuid, before: i64, now: i64) -> Result<u64, MetaError>;
   /// Sets the source's embedding dimension when it is NULL; returns the stored dimension.
   pub(crate) async fn set_embed_dimension(conn, source: Uuid, dimension: u32) -> Result<u32, MetaError>; // returns the stored dimension
   pub(crate) struct RefitRow { id, source_id, trigger: &'static str /* schedule|manual */, started_at, finished_at: Option<i64>,
       pending: u32, clusters: u32, created: u32, joined: u32, outliers: u32, cost_usd: f64, error_class: Option<String> }
   pub(crate) async fn insert_refit(conn, &RefitRow) -> Result<(), MetaError>;
   pub(crate) async fn finish_refit(conn, &RefitRow) -> Result<(), MetaError>;      // writes every counter
   pub(crate) async fn list_refits(pool, source: Uuid, limit: u32) -> Result<Vec<RefitRow>, MetaError>; // newest first
   pub(crate) async fn last_refit_started(pool, source: Uuid) -> Result<Option<i64>, MetaError>;
   /// Enabled, unpaused LLM sources whose `next_refit_at <= now`, with project and org active.
   pub(crate) async fn due_refits(pool, now: i64) -> Result<Vec<SourceRow>, MetaError>;
   pub(crate) async fn set_next_refit(conn, source: Uuid, at: i64) -> Result<(), MetaError>;
   /// Enabled, unpaused LLM sources (score, feedback, topic) of active projects and orgs, with org id
   /// and completion timeout, for the triggers.
   pub(crate) async fn llm_sources(pool) -> Result<Vec<(SourceRow, OrgId, Option<i64>)>, MetaError>;
   ```
   In `ready_candidates`, a source's `chat_connection_id` or `embed_connection_id` in `saturated` excludes its candidates. Its SQL joins `issue_sources s`, `projects p`, `orgs o` with `s.enabled = 1 AND s.paused_reason IS NULL AND p.state = 'active' AND o.state = 'active'`. Copy the state columns' names from `meta/eval_jobs.rs`'s `candidates` read.
5. `sweep_sources` keeps selecting `kind = 'error'` only (topic sources come from `llm_sources`).

**Tests** (`meta/issues.rs`):
- the new columns round-trip, with empty lists stored as NULL;
- `insert_candidate` deduplicates;
- `admit_daily` stops at the cap and starts again the next day;
- `claim_candidate`, `start_candidate_attempt` and `finish_candidate` are fenced, and a stale `claims` returns false;
- `interrupt_candidates` requeues, then fails at 3;
- `ready_candidates` skips paused sources, saturated connections and future `not_before`;
- `source_centroids` leaves out merged issues;
- `pending_candidates` and `mark_outliers`;
- `set_embed_dimension` sets once and returns the stored value afterwards;
- refits round-trip, newest first;
- `due_refits`;
- `encode_vector` and `decode_vector` round-trip, and a bad length is refused.

**Commits:** see the split above.

---

### M3.2 Limiter and config

**Goal:** IC workers can take a slot and a token of a connection from I's limiter, and `[issues] workers` exists.

**Edit:** `crates/server/src/eval/dispatch.rs`, `crates/server/src/config.rs`, `website/src/content/docs/docs/self-hosting/configuration-reference.md`.

**Steps:**
1. `dispatch.rs`: extract the "get or create the `ConnectionLimit` for `(id, limits)`" part of `pick_limits` into `fn limit_for(map: &mut HashMap<Uuid, Arc<ConnectionLimit>>, id: Uuid, limits: (u32, u32), now: i64) -> Arc<ConnectionLimit>`, used by `pick_limits`. Add:
   ```rust
   impl crate::eval::EvalQueue {
       /// A slot and a token of connection `id` for an IC call (D-IC-12), sharing the judges' limits.
       /// `None` when no slot or no token is free now; the slot is taken before the token, as `admit` does.
       pub(crate) fn try_connection_permit(&self, id: Uuid, limits: (u32, u32), now: i64) -> Option<OwnedSemaphorePermit>;
       /// Connections with no free slot or token now (what `saturated_ids` lists, without webhooks).
       pub(crate) fn saturated_connections(&self, now: i64) -> Vec<Uuid>;
   }
   ```
   `saturated_ids` uses `saturated_connections` for its connection part. If `EvalQueue`'s fields are private to `eval/mod.rs`, put the `impl` in `eval/mod.rs` instead, and report it.
2. `config.rs`: add `IssuesConfig { workers: u32 }` (`[issues]`, `#[serde(deny_unknown_fields, default)]`, default 4) as `Config.issues`. Validate `1..=64`, else `ConfigError::Invalid { key: "issues.workers", reason: "must be between 1 and 64" }`. Add tests copied from `eval.workers`' (default, range).
3. Configuration reference: a `## [issues]` section with the row `issues.workers | | integer | 4 | Workers that summarize and embed issue candidates (1–64). They share each connection's limits with judges.`. Copy the exact column format of `## [eval]`, including the env var column, which the reference test checks.

**Tests:** `try_connection_permit_shares_judge_slots` (with `max_concurrency` 1, a second permit is `None` until the first drops); `try_connection_permit_respects_the_bucket`; the config tests.

**Commit:** `Let issue workers share connection limits`

---

### M3.3 Triggers

**Goal:** bad scores and completed production traces become queued candidates (D-IC-4).

**Amendments:** A11. Also edit `crates/server/src/routes/issues.rs`, only to call `issues::triggers::refresh_score_rules(&state)` after a source create, update and delete (log errors).

**Edit:** new `crates/server/src/issues/bad_score.rs`, new `crates/server/src/issues/topics.rs`, new `crates/server/src/issues/triggers.rs`, `crates/server/src/issues/mod.rs`, `crates/server/src/issues/sources.rs`, `crates/server/src/scores.rs`, `crates/server/src/eval/trigger.rs`, `crates/server/src/http/mod.rs` (`AppInner` field), `crates/server/src/serve.rs` (field construction).

**Steps:**
1. `bad_score.rs`, a port of `web/src/lib/score-tone.ts`:
   ```rust
   /// Where `value` sits between bad (0) and good (1) under `definition`, or None (D-I-47).
   pub(crate) fn goodness(value: &ScoreValue, definition: Option<&DefinitionRow>) -> Option<f64>;
   /// A score that makes a candidate (D-IC-4): `false`, or goodness ≤ 1/3.
   pub(crate) fn is_bad(value: &ScoreValue, definition: Option<&DefinitionRow>) -> bool;
   ```
   `goodness`:
   - **Boolean**: `true` → 1, `false` → 0, with or without a definition.
   - **Numeric**: needs a definition with `min_value` and `max_value`, `min < max`, and direction `Higher` or `Lower`. `p = clamp((v − min) / (max − min), 0, 1)`; `Higher` → `p`, `Lower` → `1 − p`.
   - **Categorical**: needs direction `Higher` or `Lower` and the label's category to have a numeric `value`. Position the value between the min and max of all category values, then orient as for numeric. A single distinct value is `None`.
   - **Text**: `None`.

   `is_bad` is `goodness ≤ 1/3`, with `None` meaning not bad. An archived definition counts as none. Copy the exact branches of `score-tone.ts` and report any difference you find between it and these words.
2. `triggers.rs`: the in-memory set of score and feedback sources.
   ```rust
   #[derive(Debug, Clone)]
   pub(crate) struct ScoreRule { pub(crate) source_id: Uuid, pub(crate) kind: SourceKind, pub(crate) score_names: Vec<String>,
       pub(crate) feedback_sources: Vec<ScoreSource>, pub(crate) sample_rate: f64, pub(crate) daily_cap: u32 }
   #[derive(Debug, Default)]
   pub(crate) struct ScoreRules { map: RwLock<HashMap<ProjectId, Vec<ScoreRule>>> }  // std::sync::RwLock
   impl ScoreRules { pub(crate) fn set(&self, map: HashMap<ProjectId, Vec<ScoreRule>>); pub(crate) fn of(&self, project: ProjectId) -> Vec<ScoreRule>; }
   /// Called at the end of `scores::write`; never fails the write.
   pub(crate) async fn on_scores_written(state: &AppState, project: &ProjectRow, writer: Writer, rows: &[Score], definitions: &[DefinitionRow]);
   ```
   - `AppInner` gets `issue_score_rules: ScoreRules`, and `serve.rs` and the test state builder construct it with `Default`.
   - **Who fills the set.** `issues::sources::refresh` (called each sweeper tick by `refresh_tracked`) also builds the map from `llm_sources` for kinds `score` and `feedback`, and calls `state.issue_score_rules.set(...)`. The source routes call `issues::sources::refresh(state)` after every create, update and delete; M3.7 adds those calls.
   - **The `score` kind** takes scores of any source except `issue`.
   - **The `feedback` kind** takes `feedback_sources`: `end_user` and `human` by default, and only those two are allowed.
   - **`on_scores_written`:**
     1. Return at once when `writer` is `Writer::Issue`, or when `of(project.id)` is empty.
     2. For each live row (not deleted) with a matching rule (name in `score_names`; for `feedback`, source in `feedback_sources`; source never `issue`) that `is_bad`, and that the sampling keeps (`eval::sampling::sampled(rule.sample_rate, rule.source_id, trace, None)`; for a session target, use the trace id `TraceId` from the first 16 bytes of SHA-256(session id)), build a `CandidateRow`:
        - status `queued`, `not_before = now`;
        - target, trace, span and session from the score, and the user from the score;
        - `trace_start: None`, `score_id: Some(score.score_id)`;
        - the `dedup_key` of M3.1.
     3. In one `state.meta.write`, for each candidate: `admit_daily(conn, source, day_start(now), cap)`. At the cap, skip it. Otherwise `insert_candidate`; when the insert returns false (duplicate), there is no rollback of the daily count: rare, and accepted.
     4. Any error logs a `warn!` with the project id and the count, and returns.
     5. After a successful insert, call `state.issues_wake()` (M3.4 adds it; until then, nothing).
   - In `scores.rs` `write`, call `crate::issues::triggers::on_scores_written(state, project, writer, &rows, &definitions).await;` after the usage add, before `Ok(ids)`.
3. `topics.rs`, the topic trigger:
   ```rust
   pub(crate) async fn sweep_batch(ctx: &SweepCtx<'_>, around: i64, batch: &[Completed]) -> Result<(), ()>;
   ```
   It is called from `eval::trigger::sweep_batch` after the error sweep and before the rules, when `ctx.topic_sources` is not empty.
   - `SweepCtx` gains `topic_sources: &'a [SourceRow]`.
   - `issues::sources::ProjectSources` gains `topics: Vec<SourceRow>`, filled from `llm_sources` for kind `topic`.
   - The tracked union in `refresh_tracked` includes projects that have topic sources.
   - `issues::sources::refresh` builds both the error and topic sources and the score rules.

   Per topic source, in id order:
   1. `cetra_storage::trace_refresh` with the source filter, the batch ids, `include_eval: false`, `include_experiments: false`, `strict_exclusions: true`, `READ_TIMEOUT`.
      - An invalid filter skips this source, with the hourly warning as in `errors.rs`.
      - Another error returns `Err(())`.
   2. Rows through `api::convert::match_rows(&batches, false)`, which gives the trace id and start.
   3. Keep the rows that `sampled(rate, source.id, trace, None)` keeps.
   4. One `state.meta.write`:
      - per row, `admit_daily`, then `insert_candidate`;
      - the candidate has target `trace`, `trace_start` from the row, `score_id` `None`, and user and session `None` (the worker fills them from the loaded trace).
   5. Wake the IC workers.
4. `issues/mod.rs`: `pub(crate) mod bad_score; pub(crate) mod topics; pub(crate) mod triggers;`.

**Tests:**
- `bad_score`: a table of cases matching `score-tone.ts` (numeric higher and lower, categorical, boolean without a definition, text, archived);
- triggers:
  - a bad judge score on a project with a `score` source queues one candidate;
  - a good score queues none;
  - a score of source `issue` queues none;
  - feedback from `end_user` with a `feedback` source queues one, and `api` does not;
  - the daily cap stops at `cap`;
  - sampling 0.0001 queues nothing in 20 traces (choose ids that are known not to be sampled, or check with `sampled`);
  - the same score written twice queues one candidate;
- topics (in `eval/trigger.rs`'s tests): a completed production trace queues a candidate, a filter excludes one, a project with only a topic source is tracked, and an eval trace is never a candidate.

**Commit:** `Queue issue candidates from bad scores and new traces`

---

### M3.4 Worker (M3.4a, M3.4b, M3.4c)

**Goal:** candidates are claimed, summarized, embedded and assigned (D-IC-4, D-IC-5, D-IC-7, D-IC-12).

**Split and amendments:**
- **M3.4a** (steps 1 and 3, A2, A3), commit `Add issue prompts and share the issue join`.
- **M3.4b** (steps 2 and 4: `call_trace_id` with A10, the loop, `AppInner` fields, spawn, recover, janitor, A8's `close_open_refits` call in recover), commit `Run the issue candidate workers`. `run_candidate` is a stub that finishes `Requeue { not_before: now + 60 s }`, replaced in M3.4c.
- **M3.4c** (steps 5 and 6 with A4, A5, A6, A12), commit `Summarize, embed and assign issue candidates`.

The tests are split the same way.

**Edit:** new `crates/server/src/issues/worker.rs`, new `crates/server/src/issues/prompts.rs`, `crates/server/src/issues/errors.rs` (extract the shared join and the occurrence writer), `crates/server/src/issues/mod.rs`, `crates/server/src/eval/mod.rs` (spawn, recover, wake), `crates/server/src/http/mod.rs`, `crates/server/src/serve.rs`.

**Steps:**
1. `prompts.rs`:
   ```rust
   pub(crate) const SUMMARY_PROMPT_VERSION: u32 = 1;
   pub(crate) const SUMMARY_NAME: &str = "issue_summary";
   pub(crate) const NAME_NAME: &str = "issue_name";
   pub(crate) fn summary_schema() -> Value;   // {"type":"object","properties":{"summary":{"type":"string"},"is_issue":{"type":"boolean"}},"required":["summary","is_issue"],"additionalProperties":false}
   pub(crate) fn name_schema() -> Value;      // {"type":"object","properties":{"title":{"type":"string"},"description":{"type":"string"}},"required":["title","description"],"additionalProperties":false}
   pub(crate) fn problem_prompt(score_name: &str, score_value: &str, reason: Option<&str>, input: &str, output: &str) -> String;
   pub(crate) fn topic_prompt(user_message: &str) -> String;
   pub(crate) fn name_prompt(kind: SourceKind, summaries: &[&str], titles: &[&str]) -> String;
   ```
   Exact texts (`{…}` are the arguments, and an absent reason prints `(none)`):
   - problem:
     `"You review one interaction of an LLM application that received a bad score. Describe the failure mechanism in generic words: what went wrong and how. Do not include names, numbers, quotes or details specific to this conversation. Use at most 30 words. If the evidence shows no real problem, set is_issue to false.\n\nScore: {score_name} = {score_value}\nReason: {reason}\n\nInput:\n{input}\n\nOutput:\n{output}"`
   - topic:
     `"Write what the user wants in this request, in at most 15 generic words, without names, numbers or quotes. If the request is empty or unreadable, set is_issue to false.\n\nRequest:\n{user_message}"`
   - name (`what` is `"recurring problem"` for `score`/`feedback`, `"recurring user request"` for `topic`):
     `"These summaries describe one {what} in an LLM application. Write a short title (at most 80 characters) and a description (at most 400 characters) of what they have in common, in generic words. The title must differ from these existing titles:\n{titles as \"- t\" lines, or \"(none)\"}\n\nSummaries:\n{summaries as \"- s\" lines}"`

   `score_value` renders a boolean as `true`/`false`, a number with `{}`, a categorical value as its label, and text as the text cut to 200 characters.
   ```rust
   /// Root input and output rendered to text, each cut to 4 000 chars (D-IC-19).
   pub(crate) fn previews(spans: &[Span]) -> (String, String);
   /// The first user message of the root input (D-IC-19), cut to 4 000 chars.
   pub(crate) fn first_user_message(spans: &[Span]) -> String;
   ```
   - The root is the first span with no parent, or whose parent is not in the slice, at the earliest `(start_time, span_id)`.
   - Rendering: `messages` format renders each message as `"{role}: {text parts joined by \n}"`, one per line. `text` is as is. `json` is the compact JSON.
   - `first_user_message`, for `messages`: the first `role == "user"` message's text parts. Otherwise the rendered input.
   - Use `serde_json` on `input` with `input_format`; invalid JSON falls back to the raw text.
2. Call trace ids (in `worker.rs`):
   ```rust
   /// First 16 bytes of SHA-256(`cetra.issue-call.v1` ‖ candidate or refit id (16) ‖ claims as u32 BE ‖ call byte), all-zero → last byte 1.
   pub(crate) fn call_trace_id(owner: Uuid, claims: u32, call: u32) -> TraceId;  // A10: call as u32 BE; 1 summarize, 2 embed, 3 + naming index
   ```
3. `errors.rs` refactor:
   - extract from `write_groups` a `pub(crate) async fn join_issue(conn, issue: &mut IssueRow, occurrence_times: (i64, i64), now) -> Result<bool, MetaError>`, which applies the regression and the first and last seen, writes `update_issue` when something changed, and inserts the `regressed` event; `write_groups` calls it;
   - change `write_occurrences` to take `(state: &AppState, project: &ProjectRow, source: Uuid, occurrences: Vec<Occurrence>, now: i64)` instead of `&SweepCtx`, and update its caller.

   Behaviour does not change, and the M1 tests stay as they are.
4. `worker.rs`, the loop:
   - `AppInner` gets `issue_workers: Arc<Semaphore>` (from `config.issues.workers`) and `issue_wake: Notify`, with `pub(crate) fn issues_wake(&self)` on `AppState` that calls `notify_one`.
   - `pub(crate) async fn run_workers(state: AppState, stop: watch::Receiver<bool>)`: loop until stopped, every 1 s or on wake. Each pass:
     1. `saturated = state.eval.saturated_connections(now)`;
     2. `ready_candidates(reader, now, &saturated, free workers × 4)`;
     3. for each candidate while a worker permit is free: `claim_candidate`, then spawn `run_candidate` holding the permit.
   - `eval::spawn` spawns it unless restore mode. `eval::recover` also calls `interrupt_candidates(conn, i64::MAX, now)`. The janitor also calls `interrupt_candidates(conn, now − STALE_NS, now)`.
5. `run_candidate(state, candidate, claims)`, in this order:
   1. Load the source (`get_source`) and the project (`project_by_id`). Missing, disabled or paused: `Requeue { not_before: now + 60 s }` (a paused source's candidates wait).
   2. Deletion guard check 0, as `execute.rs` does. If it blocks: `Skipped { class: "deleted" }`.
   3. **Target load.** `trace_load::load_trace` for `trace_id` (for a session target, first look up the session's latest trace before the score with `trace_list` filtered on `session_id`, limit 1, newest first; when there is none: `Dropped`). Map errors as `execute.rs` 528–545:
      - `Saturated` → `Requeue` +5 s;
      - `NotFound` and `Busy` → load retry: `Retry` +30 s, until 3 `load_retries` (counted by `finish_candidate` on `Retry` from a load), then `Failed { class: "trace_not_found" }`;
      - the rest → `Failed { class: "permanent" }`.

      Fill `trace_start` (the root's start), and `user_id` and `session_id` from the root when missing.
   4. For `score` and `feedback`, check the source filter with `trace_refresh` on the trace (`strict_exclusions: true`). Not matching: `Dropped { cost_usd: 0 }`.
   5. If `summary` is `None`:
      1. Build the prompt: problem kinds take the score from storage with `cetra_storage::score_list` by `score_id`, and the previews; topic takes the first user message. An empty topic message: `Dropped`.
      2. **Budget**: the estimate is `budget::estimate_usd(prompt chars, 200, price)`. A source with a budget and no price is refused at create, so a missing price here means no budget check. `state.eval.budgets.try_reserve(source.id, period_start, budget, estimate)`:
         - `NotLoaded` → load `spent_between` and retry;
         - `Over` → `Skipped { class: "budget" }` and pause the source with reason `budget`.
      3. **Slot**: `state.eval.try_connection_permit(chat_connection_id, (max_concurrency, requests_per_minute), now)`. `None`: `Requeue` +5 s.
      4. `start_candidate_attempt`.
      5. `complete(CompletionInput { connection, model: chat_model, prompt, schema: &summary_schema(), name: SUMMARY_NAME, description: "Summarize the issue.", temperature: Some(0.0), max_output_tokens: 200, timeout: 60 s, trace: CallTrace { trace_id: call_trace_id(candidate, claims, 1), root_name: "cetra.issue.summarize", metadata: &[("cetra.issue.source_id", …), ("cetra.issue.candidate_id", …)], ingest, prices } })`.
      6. Commit the call trace with `execute::commit_judge_trace` (make it `pub(crate)`) and count the usage with `execute::count_judge_usage`.
      7. Settle the reservation with the actual cost (`execute::spent`'s rule), and `add_spend` in the finish write.
      8. Outcome:
         - `is_issue == false` → `Dropped { cost_usd }`;
         - otherwise `Summarized { … }`. The summary is trimmed and cut to 300 chars; empty → `Dropped`.
      9. Errors by class:
         - `Retryable` → `Retry { not_before: now + eval::retry::backoff_ns(candidate.id, attempts) }`, up to 6 attempts, then `Failed { class: "retryable" }`;
         - `Pause` → pause the source (`pause_source(conn, id, "connection", detail, now)` from M3.6; until M3.6 lands, mark it failed, and M3.6 replaces that) and `Requeue`;
         - `Permanent` → `Failed { class: "permanent" }`.
   6. **Embed**, with the same budget, slot (`embed_connection_id`), attempt and error handling:
      - `embed(EmbeddingInput { connection, model: embed_model, text: summary, dimensions: Some(512), timeout: 30 s, trace: CallTrace { call_trace_id(…, 2), root_name: "cetra.issue.embed", … } })`;
      - commit the trace and the usage;
      - the estimate is `ceil(chars / 4)` input tokens at the input price. Add `budget::input_price_usd(tokens, &PriceRow) -> f64`, which uses `price_input` only.
   7. **Dimension.** `set_embed_dimension(conn, source, dim)`. When it returns another dimension: `Failed { class: "embedding_dimension_changed" }`.
   8. **Assign**, in one meta write:
      1. `source_centroids(conn, source)`, then `cluster::nearest(&vector, centroids)`.
      2. If `similarity ≥ floor`:
         - load the issue (`get_issue_tx`);
         - `join_issue(conn, &mut issue, (trace_start, trace_start), now)`;
         - if `member_count < 500`, `add_to_centroid`;
         - `member_count += 1`;
         - `update_issue`;
         - `finish_candidate(Assigned { … })`.
      3. Else `finish_candidate(Pending { … })`.
   9. After an `Assigned` write, write the occurrence with `write_occurrences(state, &project, source.id, vec![Occurrence { … }], now)`. A write error logs, and does not undo the assignment: the occurrence is missing in that case, which is accepted and logged with the ids.
   10. Logs: one `debug!` per candidate with the ids, the final status and the error class.
6. Never log a provider message. `EvalError`'s `Display` is safe, but the `Provider` variant's message is not: use `error.class()` and a short static class string.

**Tests** (`worker.rs`, using the mock provider support the server's eval tests use; read `eval/execute.rs`'s tests to find it):
- a bad score candidate is summarized, embedded and left `pending` when no issue exists, with both call traces committed in `cetra-eval`;
- with an existing issue whose centroid matches (mock embedding equal to the centroid), the candidate is `assigned`, the occurrence score is written, `member_count` is 1 more, and the centroid moved;
- `is_issue: false` → `dropped`;
- a retryable 429 retries with backoff, then succeeds;
- a 401 pauses the source (once M3.6 exists; until then, failed);
- the budget at 0.0000001 skips with `budget`;
- a saturated chat connection requeues without spending an attempt;
- an embedding failure after a summary retries without a second summary call (the mock records one chat request);
- dimension change → failed;
- `previews` and `first_user_message` on messages, text and JSON inputs;
- the prompt texts are pinned by a unit test (the exact strings above);
- `call_trace_id` is deterministic and never all zeros.

**Commits:** see the split above.

---

### M3.5 Refit

**Goal:** due sources cluster their pending candidates into new issues (D-IC-8).

**Amendments:** A3, A8, A9, A10.

**Edit:** new `crates/server/src/issues/refit.rs`, `crates/server/src/issues/mod.rs`, `crates/server/src/eval/mod.rs` (spawn), `crates/server/src/issues/worker.rs` (only to reuse its helpers).

**Steps:**
1. `pub(crate) async fn run_refits(state: AppState, stop: watch::Receiver<bool>)`: every 60 s, `due_refits(reader, now)`, one at a time (the loop itself is the process-wide semaphore of 1), skipped in restore mode.
2. `pub(crate) async fn refit_source(state: &AppState, source: &SourceRow, trigger: &'static str, now: i64) -> Result<RefitRow, MetaError>`:
   1. Insert the refit row (id `Uuid::now_v7()`, `started_at = now`).
   2. `mark_outliers(conn, source, now − 7 days, now)` into `outliers`.
   3. `pending_candidates(conn, source, now − 7 days, 5 000)`. `pending` is the count. With fewer than 20, finish the refit, set `next_refit_at = now + 6 h` and return.
   4. Order the candidates by `(created_at, id)` ascending, as D-IC-17 requires. Collect the vectors; candidates whose vector is gone or has the wrong dimension are left out.
   5. `tokio::task::spawn_blocking(move || cluster::average_linkage(&vectors, 1.0 − floor as f32))`. An error ends the refit with `error_class: "cluster"`.
   6. Keep the clusters of at least `min_issue_size` members. Sort them by size descending, then by first index. `clusters` is their count.
   7. Per kept cluster:
      1. The centroid is the normalized mean of its members (`l2_normalize` of the sum).
      2. `source_centroids` → `nearest`. If the similarity is at least the floor, join that issue:
         - `join_issue` with the min and max `trace_start` of the members, `merge_centroids(issue.centroid, member_count, cluster centroid, n)` and `member_count += n`;
         - `set_candidates_assigned`;
         - `joined += n`;
         - occurrences for each member, through `write_occurrences`.
      3. Else name it:
         1. The 20 members nearest the centroid (by `cosine`), and the 50 newest titles of the source's issues (any state: `list_issues` with all states, limit 50).
         2. `name_prompt`. Take a slot on the chat connection, retrying every 1 s for up to 120 s; at the timeout, end the refit with `error_class: "busy"`.
         3. The budget, as in the worker. `Over` ends the refit with `budget` and pauses the source.
         4. `complete(… name: NAME_NAME, max_output_tokens: 300, root_name: "cetra.issue.name", call_trace_id(refit id, 0, 3 + index) …)`. Commit the trace and the usage, and add the spend.
         5. An error ends the refit with its class (`Pause` also pauses the source).
         6. Trim the title and cut it to 80 characters; cut the description to 400. A title equal (case-insensitive) to an existing one gets `" (2)"`, `" (3)"`, … appended.
         7. Insert the issue:
            - state `open`;
            - `first_seen` and `last_seen` from the members' trace starts;
            - the centroid, and `member_count = n`;
            - a `created` event with no actor;
            - `set_candidates_assigned`.

            `created += 1`. Write the members' occurrences.
   8. Finish the refit with its counters and `cost_usd`, and set `next_refit_at = now + 6 h` (the end of the refit).
3. Logging: one `info!` per refit with the source id, the counters and the error class.

**Tests** (`refit.rs`, with the mock provider):
- 25 pending candidates in two tight groups (12 and 13) plus 0 noise give two new issues with the mocked titles; their candidates are `assigned` with occurrences, and the refit row counts 2 created;
- a group near an existing issue joins it instead of naming;
- fewer than 20 pending → no clustering, and a refit row with `pending` < 20;
- a cluster below `min_issue_size` stays pending, and is an outlier after 7 days;
- a duplicate title gets `(2)`;
- a naming error ends the refit with its class, and the issues created before stay;
- `next_refit_at` moves 6 h after the refit;
- determinism: the same pending set twice gives the same clusters.

**Commit:** `Refit issue sources into new issues`

---

### M3.6 Pauses, budgets, pruning, deletion and the connection test (M3.6a, M3.6b)

**Goal:** sources pause and resume with their connections and budgets; old candidates and deleted traces' candidates go away; the connection test covers embeddings.

**Split and amendments:**
- **M3.6a** (steps 1–3, A7, with `eval/execute.rs` for A2's `connection_unchanged`), commit `Pause and prune issue sources`.
- **M3.6b** (steps 4–6), commit `Cover issue sources in the connection test and deletions`.

**Edit:** `crates/server/src/meta/issues.rs`, `crates/server/src/issues/worker.rs`, `crates/server/src/issues/refit.rs`, `crates/server/src/eval/dispatch.rs` (budget and prune ticks), `crates/server/src/routes/connections.rs`, `crates/server/src/api/connections.rs`, `crates/server/src/meta/evaluators.rs`, `crates/server/src/deletion/resolve.rs`, `docs/spec/03-storage.md` (§ Cascades: one line), `docs/spec/09-api.md` (connection test body).

**Steps:**
1. `meta/issues.rs`:
   - `pause_source(conn, id, reason: &str /* connection|budget|errors */, detail: Option<&str>, now) -> Result<(), MetaError>`;
   - `pause_sources_of_connection(conn, connection: Uuid, detail: Option<&str>, now) -> Result<Vec<Uuid>, MetaError>`: sources whose chat or embedding connection is that id;
   - `resume_connection_pauses(conn, connection: Uuid) -> Result<Vec<Uuid>, MetaError>`: clears `paused_reason = 'connection'` for the sources using it;
   - `clear_budget_pauses(conn, day_start, month_start, now) -> Result<u64, MetaError>`: as `online_rules::clear_budget_pauses`, for sources;
   - `delete_candidates_of_deletion(conn, project, deletion_id) -> Result<u64, MetaError>`: `DELETE FROM issue_candidates WHERE project_id = ? AND trace_id IN (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?)`;
   - `prune_candidates(conn, now) -> Result<u64, MetaError>`:
     1. NULL the `vector` of `pending` candidates older than 7 days and of `assigned` ones older than 30 days;
     2. delete candidates older than 30 days;
     3. delete `issue_source_days` older than 62 days;
     4. keep the newest 50 refits per source.
   - `prune_to_window(conn, org, visible_from) -> Result<u64, MetaError>`: delete candidates whose `trace_start < visible_from` in that org's projects.
2. `worker.rs` and `refit.rs`: a `Pause` error calls `pause_sources_of_connection` for the failing connection, plus `connections::set_connection_status(conn, id, "failing", None, now)`, in the finish write, with the D-I-14 fence (`connection_unchanged`, made `pub(crate)` in `execute.rs`) as the judge path does. Replace the M3.4 placeholder failure.
3. `dispatch.rs`:
   - `budget_once` also calls `issues::clear_budget_pauses`;
   - `prune_once` also calls `prune_candidates` and, per active org with a data window (`Visibility::window(state, org).visible_from()`), `prune_to_window`.
4. `meta/evaluators.rs` `connection_in_use`: also true when an `issue_sources` row uses the connection as chat or embedding connection.
5. Connection test:
   - `TestLlmConnectionRequest` gains `#[serde(default)] embedding_model: Option<String>`. When it is set, the handler checks that the adapter is not Anthropic (otherwise the answer is `ok: false`, class `permanent`, message "This provider has no embeddings API.");
   - it runs `state.eval_runner.embed(EmbeddingInput { connection, model, text: "ping", dimensions: None, timeout: 30 s, trace: CallTrace { random id, root_name: "cetra.issue.embed", metadata: &[], ingest (nil project, as the chat test), prices } })` and discards the trace;
   - the `model` field of the result is the embedding model;
   - success: `recover_connection` as for the default model, and `issues::resume_connection_pauses`;
   - a `Pause` class sets the connection `failing` as now.

   The chat path's success with the default model also calls `issues::resume_connection_pauses`, and so does the key-update route next to `online_rules::resume_connection_pauses`. Each then wakes the IC workers.
6. `deletion/resolve.rs`: in the `if stored` block, after `delete_jobs`, call `crate::meta::issues::delete_candidates_of_deletion(conn, project, id)`. Add the line to spec 03 § Cascades: "Issue candidates of the deleted traces (track IC)."

**Tests:**
- a connection pause pauses the sources that use it, and a successful embedding test resumes them;
- `connection_in_use` with a source;
- the budget pause is cleared at the next day;
- prune drops old vectors and rows;
- prune to the window;
- a trace deletion deletes its candidates;
- the connection test with `embedding_model`: OpenAI ok, Anthropic refused.

**Commits:** see the split above.

---

### M3.7 API

**Goal:** sources of the LLM kinds through the API, refit history and "Refit now".

**Edit:**
- `crates/server/src/api/issues.rs`, `crates/server/src/routes/issues.rs`;
- `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`;
- fixtures: `static/issue-sources.json` (add a topic source), new `static/issue-refits.json`, new `requests/issue-source-create-topic.json`;
- `web/src/mocks/writes.ts`;
- every file where M3 put a dead-code attribute, to remove it.

**Steps:**
1. `IssueSourceKind` gains `Score`, `Feedback` and `Topic`. `IssueSource` gains, all optional in JSON (absent for `error`):
   - `sample_rate`, `daily_cap`, `score_names`, `feedback_sources`;
   - `chat_connection_id`, `chat_model`, `embed_connection_id`, `embed_model`, `embed_dimension`;
   - `floor`, `min_issue_size`;
   - `budget_usd`, `budget_period` (`day`/`month`, reusing online rules' `BudgetPeriod`);
   - `paused_detail`, `next_refit_at`;
   - `candidates_24h: { queued, pending, assigned, dropped, failed, skipped }`;
   - `spent_today_usd`.
2. Create and update requests gain the same input fields. Validation, all field errors at once:
   - **`error` kind**: any LLM field is a 400 `"not used by error sources"`.
   - **LLM kinds** require `chat_connection_id`, `chat_model`, `embed_connection_id` and `embed_model`. Defaults: `sample_rate` 1.0 (range `(0, 1]`), `daily_cap` 500 (1–10 000), `floor` 0.80 (0.5–0.99), `min_issue_size` 5 (2–100).
   - **`score` and `feedback`** require `score_names` (1–8 names, each 1–255 bytes). `feedback_sources` is only for `feedback`: `end_user` and/or `human`, default both.
   - **The filter** is a `traces` expression for the LLM kinds (`validate_filter(…, ViewName::Traces, …)`); `topic` defaults to `environment = 'production'` when absent at create.
   - **Connections** must exist in the org. An Anthropic embedding connection is a 400 `"this provider has no embeddings API"`.
   - **Budget:** `budget_usd` needs `budget_period`, and the chat model must have input and output prices, and the embedding model an input price, else a 400 (`budget::model_price`, and the new input-only check).
   - **PATCH** refuses `embed_connection_id` and `embed_model` (400 `"fixed after create; create a new source"`).
   - Create sets `next_refit_at = now + 1 h` for the LLM kinds.
3. The source routes already call `refresh_score_rules` (M3.3). After a create, update or delete, also wake the IC workers.
4. New routes (tag `issues`):
   - `POST …/issue-sources/{id}/refit` (`ManageEvaluation`): 400 for an `error` source, 409 when `last_refit_started` is less than 10 minutes ago, else `set_next_refit(now)` and `202` with the source;
   - `GET …/issue-sources/{id}/refits` (`View`): `{ data: [IssueRefit] }`, the newest 50.

   ```rust
   pub struct IssueRefit { pub id: Uuid, pub trigger: String, pub started_at: i64, pub finished_at: Option<i64>,
       pub pending: u32, pub clusters: u32, pub created: u32, pub joined: u32, pub outliers: u32,
       pub cost_usd: f64, pub error_class: Option<String> }
   ```
   Times use the same serialization as the other issue types (`time_serde`).
5. OpenAPI paths and schemas, test rows, fixtures with ids `…1c20` and up, `api_examples` for the refits GET, and the mock handlers for `post_issue_source_refit`.
6. Remove M3's dead-code attributes.

**Tests:**
- create a topic source with defaults, including its filter default;
- create a score source without `score_names` → 400;
- an Anthropic embedding connection → 400;
- PATCH of the embedding model → 400;
- an LLM field on an error source → 400;
- refit route: 202, then 409 within 10 minutes, and 400 on an error source;
- the refits list;
- `candidates_24h` counts.

**Commit:** `Add LLM issue sources to the API`

---

### M3.8 Web and docs

**Goal:** the source editor handles the LLM kinds, the source page shows refits, and the docs describe them.

**Edit:**
- `web/src/components/issue-sources/source-form.tsx` (+ test), `web/src/components/issue-sources/source-list.tsx`;
- new `web/src/routes/$org/$project/issue-sources/$sourceId/index.tsx` (+ test);
- `web/src/api/queries-issues.ts`;
- `web/src/routes/$org/$project/issues/index.tsx` (a kind filter);
- `website/src/content/docs/docs/evaluation/issues.mdx`;
- `docs/spec/06-evaluation.md` § Issues, `docs/spec/09-api.md` § Issues, `docs/spec/11-ui.md` § Issues;
- `CHANGELOG.md`, `docs/roadmap.md`.

**Steps:**
1. Form:
   - The kind select offers Errors, Bad scores, User feedback and Topics.
   - For the LLM kinds:
     - score names: a multi-select of the project's score definitions plus free text, for `score` and `feedback`;
     - feedback sources: checkboxes, for `feedback`;
     - chat connection and model, and embedding connection and model. Connections come from the org connections query the evaluator form uses. Anthropic connections are disabled in the embedding select, with the hint "No embeddings API". The embedding fields are read-only on edit, with the hint "Fixed after create. Create a new source to change them.";
     - sampling (%), daily cap, budget (amount and period), floor, minimum issue size; the floor and minimum size sit under "Advanced", collapsed;
     - the filter editor in traces mode.
   - Server field errors show under their fields.
2. Source page (`$sourceId/index.tsx`):
   - name, kind, state (paused with its reason and detail, as on the online-rule page);
   - today's spend against the budget;
   - candidate counts for 24 h;
   - "Refit now" (a 409 shows "A refit started less than 10 minutes ago.");
   - the refit history table: started, duration, pending, clusters, created, joined, outliers, cost, error;
   - an Edit link.

   List rows link to it.
3. Issues list: a kind filter (All, Errors, Bad scores, User feedback, Topics). It filters on the client by the source's kind, from the sources query.
4. Docs page `issues.mdx`, a new section "Grouping bad scores, feedback and topics":
   - how candidates are made;
   - that summaries and embeddings run on your LLM connections and cost tokens;
   - the budget, the cap and sampling;
   - refits every 6 h, and Refit now;
   - the floor and the minimum size;
   - that embedding models are fixed per source;
   - Anthropic connections cannot embed.

   Follow `website/STYLE.md`.
5. Spec sections; `CHANGELOG.md` `### Added`: `- Issue sources for bad scores, user feedback and topics: LLM summaries and embeddings group them into issues, refreshed every 6 hours (D-IC-4, D-IC-8).`; roadmap IC row: M3 implemented.

**Tests:** form for each kind (required fields, the read-only embedding on edit, the Anthropic option disabled); source page renders refits; Refit now posts and shows a 409; the issues kind filter.

**Commit:** `Add LLM issue sources to the web UI and docs`

---

## Closing

After M3.8:
1. regenerate `just openapi`, `just api-examples` and `just docs-gen`, and accept the insta snapshots after reading them;
2. run `just check` once and fix what fails, with Haiku fix briefs;
3. a Sonnet review against this plan and the design, whose findings are fixed and checked.
