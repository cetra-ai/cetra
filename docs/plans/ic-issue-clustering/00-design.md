Status: design, in review with the user (2026-11-20). No milestone plan is written yet.

# IC · Issue clustering — design

Track IC finds recurring problems in production traces and groups them into named **issues** that users browse, track over time, resolve, and link to alerts and datasets. The user moved it into the MVP on 2026-11-20 (D194, amending D20).

**Read first:** `AGENTS.md`; `docs/mvp-decisions.md` (D20, D21, D25, D173–D179, D194, Track I: D-I-4, D-I-6–D-I-18, D-I-25, D-I-31, D-I-35, D-I-44, D-I-47; Track K: D-K-2–D-K-4, D-K-11, D-K-14; Track P: D-P-12, D-P-13; Track IC below); specs 01 § Score, 04 § Evaluation and § Alerts, 05 § Filter DSL (score functions), 06, 07, 09, 11; the competitor notes in § Competitors.

This document holds the goals, the architecture, the contracts between parts, the decisions and the milestones. Each milestone gets its own plan, written just before it starts from the code that exists then. **Haiku implements the plans one phase at a time; Sonnet reviews each milestone.**

## Goals

- **Four kinds of signal** produce issue candidates (user, 2026-11-20):
  - errors (spans with `status = 'error'`);
  - bad evaluation scores (failed checks, judge scores in the bad third of their definition, D-I-47);
  - negative user feedback (`end_user` and `human` scores in the bad third, or a boolean at its bad value);
  - topics (sampled production traces, grouped by what the user asked).
- **Stable issues.** An issue keeps its id, title, history, state and links for life. New occurrences join existing issues, and a new problem becomes a new issue within minutes to hours, without a full recompute that would renumber everything.
- **A lifecycle** that respects human decisions: open, resolved, ignored, merged. A resolved issue that recurs is reopened and marked regressed. Nothing automatic overrides a state that a person set, except that regression reopen.
- **Tracking over time**: counts per day, first and last seen, affected users, example traces, and a filter on the trace list for an issue's traces.
- **Links**: alerts on an issue's occurrence count and on new or regressed issues; "add examples to a dataset"; "send to an annotation queue" when track AQ lands.
- **Cost control**: per-source sampling, a daily candidate cap, a USD budget, the units cap, and the org's own LLM connection paying for every call (D178).
- **One binary**: no vector database, no Python, no external service (D-IC-6).

## Non-goals (MVP)

- Re-clustering existing issues: no automatic split, and no automatic merge of two existing issues. A person merges them (D-IC-9).
- One issue fed by several sources. Each source clusters into its own issues (D-IC-3).
- A vector index, semantic search over traces, or "find similar traces". D25 keeps full-text search a scan, and vectors here serve clustering only.
- A local embedding model in the binary. The user chose the org's connection (D-IC-5). The design does not preclude adding one later as another embedding provider.
- 2D maps, treemaps and hierarchies of topics (Langfuse, Laminar). The MVP has one level.
- LLM-written fix suggestions ("fix prompt" for coding agents, MLflow), and agentic root-cause analysis (Opik).
- Issue assignment to people, comments and external trackers (Jira, Linear, GitHub).
- Backfill: a source sees traces and scores from its creation on.

## Competitors

Full notes: the shared notes file `ic-competitors.md` (the session-notes folder of the design sessions, not in the repo). The checkouts are from 2–9 November 2026.

| | Laminar Signals | LangWatch Topics | Opik Diagnostics | MLflow Issues | Langfuse Topics |
|---|---|---|---|---|---|
| Input | LLM finding of a user "signal" judge | trace input text | hosted agent reads traces | 100 sampled sessions | LLM facet summary per trace or session |
| Embeddings | yes | yes (user's provider) | no | no | yes (user's model slot) |
| Grouping | incremental centroid tree | Ward linkage, then p95 radius | hosted LLM agent | LLM groups labels, LLM matches stored issues | UMAP + HDBSCAN fit, then centroid radius |
| Identity across runs | persistent centroids | none (a batch replaces all) | the agent reuses ids | LLM matcher | membership overlap ≥ 80 % |
| Vectors | centroids in ClickHouse | centroids in Postgres JSON | — | — | per summary, in ClickHouse |
| Lifecycle | none | none | open, resolved, closed | pending, resolved, rejected | none |
| Alerts | new cluster | — | — | — | — |
| Trigger | continuous | daily + manual | daily + manual | schedule + manual | rules + manual fit |

Lessons kept in this design:
- **Embed an LLM-written summary of the problem, not the raw input.** The summary names a mechanism in generic words. Raw input gives topics, not issues (Langfuse, Laminar). Topics are the exception, and their summary is the user's intent (D-IC-4).
- **Stable ids by construction.** Existing issues never move, and a refit only creates new ones (D-IC-9). LangWatch's replace-all batch breaks every link.
- **Lock the embedding model per source** and store its name and dimension next to every vector (Laminar migrated from 3072 to 768 dimensions, D-IC-5).
- **No vector index.** Brute-force cosine over the centroids of one source is enough (Laminar dropped HNSW, D-IC-6).
- **Outliers are first-class**, and new issues need a minimum size (Langfuse, Laminar; D-IC-8).
- **Never overwrite people**: no reopen of an ignored issue, and a regression reopens visibly (MLflow, Opik; D-IC-10).
- **Bound every prompt**: the namer sees at most 20 summaries and 50 existing titles, so a long-lived project never overflows the context (MLflow's overflow filed duplicates; D-IC-8).
- **Runs are observable**: each refit is recorded with its counts, cost and error class, and zero-result and failed runs are kept too (LangWatch, Langfuse; D-IC-12).
- **Errors need no LLM.** None of the five groups errors deterministically. Sentry-style fingerprints are free, exact and stable, and they work for orgs without a connection (D-IC-2).
- Nobody links issues to alerts, datasets or annotation queues, and only Laminar alerts on new clusters. cetra does all of these.

## Decisions

The user's answers (2026-11-20):

| Question | Answer |
|---|---|
| Embedding provider | The org's LLM connection (an embeddings call added to the OpenAI-compatible and Gemini adapters). No local model in the binary. |
| Input signals | All four: errors, bad evaluation scores, negative user feedback, topics. |
| Trigger | Continuous: per-project sources like online rules (filter, sampling, budget), candidates assigned within minutes, and a refit every few hours and on demand that makes new issues. |
| Plan gating | Free gets 1 active issue source and paid plans unlimited, as for online rules (D178). The code stays MIT. |

The decisions below are `D-IC-n` in `docs/mvp-decisions.md` § Track IC. This section explains them; the log holds their one-line form.

### D-IC-1 Sources

An **issue source** is a per-project rule with:
- a `kind`: `error`, `score`, `feedback` or `topic`;
- a `name` that is unique in the project and becomes part of the issues' display;
- an optional filter, parsed by storage at create and `PATCH` (`invalid_filter`, D-I-30):
  - for `error`, a `spans` DSL expression, ANDed with `status = 'error'` by the read;
  - for the other kinds, a `traces` DSL expression. A `topic` source defaults to `environment = 'production'`.
- for the LLM kinds only (`score`, `feedback`, `topic`):
  - `sample_rate` in `(0, 1]`, sampled by SHA-256 as in D-I-10 with the label `cetra.sample.v1` and the source id;
  - `daily_cap`, the maximum number of candidates accepted per UTC day (default 500, 1–10 000), counted in `issue_source_days` so it survives restarts.
  An `error` source has neither: every error span is fingerprinted, so a new error always becomes an issue (D-IC-2). Sending one for an `error` source is a 400.
- for `score` and `feedback`, the score names it watches (1–8), and for `feedback` the score sources it accepts (`end_user`, `human`, default both);
- for the LLM kinds (`score`, `feedback`, `topic`): a chat connection and model, an embedding connection and model, and an optional `budget_usd` per `day` or `month` (D-I-15's rules);
- `enabled` and `paused` with a pause reason, as for online rules (D-I-14 and D-I-44's pause classes).

A project can have any number of sources, within the plan limit (D-IC-13).

### D-IC-2 Error issues are fingerprinted, not clustered

An `error` source makes no LLM or embedding call. For each error span of a completed trace that passes the filter and the sampling, it computes a **fingerprint**: the first 16 bytes of SHA-256 over `cetra.issue-fp.v1`, the span's `service_name`, its `name`, the exception type, and the normalized message. The exception type and message come from the last `exception` event, else from `status_message`.

Normalization replaces these with placeholders, in this order:
- URLs, reduced to their scheme and host;
- email addresses;
- UUIDs;
- ISO 8601 timestamps;
- IPv4 and IPv6 addresses;
- file paths (two or more `/` or `\` separated segments);
- hex runs of 8 or more characters containing at least one letter and one digit;
- quoted strings longer than 2 characters;
- numbers.

Then it collapses whitespace and cuts at 512 characters. The plan pins the exact rules and their golden tests.

A fingerprint maps to exactly one issue of the source, and a new fingerprint creates one at once. The title is `<exception type or span name>: <normalized message>`, and the user can rename it. The source has no refit.

Error spans are read through track I's completion sweeper (D-I-8, D-I-9):
- the tracked set grows to include projects with an enabled error source;
- a new storage read, `error_spans`, takes the same batch, filter and exclusions as `span_refresh`. It returns, for at most 50 error spans per trace (earliest first): `trace_id`, `span_id`, `start_time`, `service_name`, `name`, `status_message`, and the type and message of the last `exception` event.
- `span_refresh` returns ids only, which is why the new read exists. It ships in M1.
- Within a batch the error sweep runs before the online rules. In a project with no online rules, a failed error sweep retries the batch. Otherwise it skips that batch's error issues and never stops the online rules: the rule enqueue spends the hourly cap and is not idempotent, so the batch is retried only when the rules fail, and a retry re-runs the idempotent error sweep.

The normalizer removes the common personal data shapes, but a message can still hold a name or another identifier in plain words. Titles and reasons of error issues are payload-derived data (§ Logs and privacy), never logged.

### D-IC-3 Issues belong to one source

Every issue belongs to exactly one source, and clusters only with that source's candidates. Deleting a source deletes its issues and their candidates; the occurrence scores stay until their traces go (D-IC-11). Issues of different sources of one project are listed together and can be filtered by source. Cross-source issues are a non-goal.

### D-IC-4 Candidates and summaries

A **candidate** is one target accepted by an LLM source. It is processed in two calls on the source's connections:
1. **Summarize.** One structured-output chat call returns `{summary, is_issue}`, from a built-in prompt per kind with fixed size limits:
   - `score` and `feedback` kinds see the score (name, value, reason or comment) and the target's input and output previews, cut to 4 000 characters each. They describe the failure mechanism in generic words, without names, numbers or quotes from the conversation, in at most 30 words.
   - `topic` sees the first user message and the root input preview, and writes the user's intent in at most 15 words.
   - `is_issue = false` drops the candidate. This happens when the judge's reason shows no real problem, or when the topic input is empty.
2. **Embed.** One embeddings call on the summary.

The prompts live in code, versioned (`summary_prompt_version` on each candidate). Editing them is out of scope for the MVP. The calls are traced like judge traces: environment `cetra-eval`, not billable (D-I-31), and linked from the candidate. The summary call reuses `cetra-eval`'s judge adapters and structured-output modes (`StructuredOutput`), so the three providers work for it. Embeddings need an OpenAI-compatible or Gemini connection; an Anthropic connection cannot be picked for the embedding slot.

Triggers:
- `score` and `feedback` candidates come from **score writes**. At the end of a successful `scores::write` (one place, so the API, the SDK, end-user feedback, annotations and evaluators all pass through it), `issues::on_scores_written` looks at the written rows. It returns at once for a project without an enabled `score` or `feedback` source, using an in-memory set rebuilt like I's tracked set. It queues a candidate for a live score when all of these hold:
  - the score's name and source match a source;
  - its value is bad: a numeric or categorical value in its definition's bad third, in the definition's direction (D-I-47), or a boolean at the value its direction makes bad (`false` under `higher`, `true` under `lower`). A boolean with no live definition is bad when it is `false`. Scores with direction `none`, and scores that their definition cannot place (no range, another type, an unknown category), never qualify.
  - the sampling and the daily cap pass.
  The source's `traces` filter is checked by the worker before the first call, with one `trace_refresh` read on the target trace. A trace that does not match ends `dropped`. Scores of source `issue` never reach this check (D-IC-11).
- The input of a session-target score is its comment or reason, plus the input and output previews of the session's latest trace that started before the score.
- `topic` candidates come from **trace completion**, through the sweeper as for error sources.
- Both skip `cetra-eval` traces and experiment traces, as online rules do (D-I-17).
- Perf: a score candidate's first load has no trace start, so it reads every file of the project; later attempts read from the stored trace start.

### D-IC-5 Embeddings come from the org's connection, locked per source

The embedding connection and model are fixed when the source is created and cannot change afterwards. Changing them means a new source, because vectors of two models are not comparable. A budgeted source needs a price for both models, else 400 (D-I-15's rule). The price table may lack an embedding model; then the source runs without a budget, and its cost shows as unknown. Each vector is stored with its model name and dimension, and a vector whose dimension differs from the source's first one fails the candidate (`embedding_dimension_changed`). The call requests no size (no `dimensions`, no `outputDimensionality`): the provider's native size is kept, the dimension of the source's first answer is stored, and at most 4 096 are accepted (a larger answer fails the source with a pause). Embedding calls are priced from the price table's `input` price (D82) and count toward the source's budget.

### D-IC-6 Vectors live in `meta.db`, scanned brute force

- **Centroids.** Each issue of an LLM source keeps its centroid (f32 little-endian BLOB) and its member count in `meta.db`. A centroid is a mean of many embeddings and holds no readable text. It is not recomputed when members are deleted, and this is accepted.
- **Candidates.** Each candidate keeps its vector while it is useful: unassigned candidates for 7 days (the refit pool), assigned ones for 30 days (the namer's examples). An hourly pruner deletes older vectors and keeps the candidate row (summary, issue, target) for 30 days in total, or less when the trace leaves the org's data window (D175): candidates whose `trace_start` is older than the window are deleted.
- **No index.** Assignment compares one vector with every open, resolved or ignored issue centroid of the source. A refit loads the source's unassigned vectors, at most 5 000 (D-IC-8).
- **Size.** The daily cap bounds storage: 500 candidates × 1 536 × 4 bytes × 30 days ≈ 92 MB per source at the default, for a model of 1 536 dimensions; at the 4 096 limit it is ≈ 246 MB.

Rejected:
- an external vector database (single binary);
- a vector column in the Parquet store (append-only, LWW, and spec 03 has no update path for centroids);
- a separate SQLite file (backups, restore and deletion would need a second database).

### D-IC-7 Incremental assignment

When a candidate has its vector:
1. Take the nearest issue centroid of its source by cosine similarity.
2. If the similarity is at least the source's `floor` (default 0.80, 0.5–0.99), the candidate joins that issue. The floor is the only threshold: the refit uses it too (D-IC-8), so a candidate the refit would put in an issue also joins it online. Then:
   - the centroid moves by a running mean, but only until the issue has 500 members, so it does not drift forever;
   - the issue's `last_seen` and counters update, and the occurrence score is written (D-IC-11).
3. Otherwise the candidate stays `pending` for the next refit.

A merged issue is never a target: its members go to the issue it was merged into (D-IC-9).

### D-IC-8 Refit makes new issues only

Every 6 hours per source (the first time 1 hour after the source is created), and on demand (`POST …/issue-sources/{id}/refit`, at most once per 10 minutes per source), the refit runs. It is skipped when there are fewer than 20 pending candidates.
1. Load the source's pending candidates of the last 7 days, newest first, at most 5 000.
2. Cluster them with average-linkage agglomerative clustering on cosine distance (the nearest-neighbour-chain algorithm, O(n²) time), cut at `1 − floor`, on a blocking thread (`spawn_blocking`). This is deterministic and needs no parameter beyond the floor. Its distance matrix is the condensed upper triangle (≈ 50 MB of f32 at 5 000 candidates), and refits take a process-wide semaphore of 1, so only one matrix exists at a time.
3. Keep clusters of at least `min_issue_size` members (default 5, 2–100). Smaller ones stay pending; after 7 days a pending candidate becomes `outlier`.
4. For each kept cluster, in size order:
   - if its centroid is within the floor of an existing issue of the source (any state but merged), its members join that issue (step 2 of D-IC-7);
   - otherwise one naming call (the source's chat connection) receives the 20 summaries nearest the centroid and the titles of the source's 50 most recent issues. It returns `{title (≤ 80 chars), description (≤ 400 chars)}`, and the title must differ from the listed titles.
   - a new issue is created with its centroid and members, and a `created` event.
5. The refit is recorded (D-IC-12).

Existing issues are never split, merged or renamed by a refit. A person renames, merges or ignores them. The LLM never sees more than 20 summaries and 50 titles per call, so the prompt stays bounded however old the project is.

### D-IC-9 Merge

`POST …/issues/{id}/merge {into}` merges an issue into another of the same source:
- the merged issue's state becomes `merged`, with `merged_into` set;
- its future candidates go to the target;
- for an LLM source the target's centroid becomes the member-weighted mean of both;
- merging into an issue that is itself merged is a 409, so chains form only from issues merged earlier into the one now merged. `merged_into` is rewritten to the final target for every issue of the chain, which keeps every chain one level deep.

Past occurrence scores keep their old issue name. The issue's set of names is the issue plus every issue merged into it:
- counts, charts and K's condition read the whole set (storage reads take a name list, at most 1 000);
- "View traces" builds a `has_score(…) OR …` filter of at most 8 names (D97): the issue and its 7 merged issues with the most occurrences, and the page says when older merged issues are left out.

There is no unmerge; renaming and ignoring cover the other mistakes. Error issues merge too: the fingerprint then maps to the target.

### D-IC-10 Lifecycle

States: `open`, `resolved`, `ignored`, `merged`.
- A person sets `resolved` and `ignored`, and can reopen either.
- **Regression.** A new occurrence on a `resolved` issue sets it back to `open`, marks it `regressed`, and records a `regressed` event. Only occurrences whose trace started after `resolved_at` count.
- **Ignored** issues keep counting occurrences but are hidden from the default list and never alert.
- `merged` is final.
- Every change is an `issue_events` row with an actor (a user, or `system` for created and regressed; a merge records the user who merged), kept for the issue's life.

### D-IC-11 Occurrences are scores

Each assignment writes one score through `scores::write`:
- source `issue` (a new `ScoreSource` value in `cetra-core`, D-IC-15);
- name `issue:<issue id>`, data type `boolean`, value `true`;
- target: the candidate's trace (the session for a session-target feedback score). The span id, the source id and the candidate id go in the metadata, and the reason is the summary (or the normalized error message);
- a new `Writer::Issue { source_id }`: the derived score id's author is the source id, so rewriting the same (trace, issue) is idempotent;
- `created_at` is the trace's start time (the candidate's `trace_start`), not the write time, so daily counts follow when the problem happened. A join made by a refit hours later lands in the past: K's windows see only joins made inside the window (accepted, D-IC-14).
- API and SDK writers cannot use source `issue`: `scores::write`'s existing mapping refuses it.

This gives, with no new storage:
- per-day counts and affected users from a scores widget or `alert_value`;
- the trace-list filter `has_score('issue:<id>')`;
- SQL over `scores`;
- deletion cascades: a deleted trace or user takes its occurrences with it (track M);
- the data window (D175).

**Units.** Occurrence scores are not billable units, and the units cap does not refuse them. They are derived by cetra from data already billed, like judge-trace spans (D-I-31). An error storm or a re-swept trace must not bill the customer. `scores::write` leaves rows of source `issue` out of `usage_daily.scores` and out of its cap check. The user confirmed this (D-IC-26).

**Hiding.** Issue scores never trigger a candidate. Source `issue` is left out of three reads, each of which filters `source <> 'issue'`: the latest-scores read, a user's score list and the dashboard scores source. `GET /scores` and the SQL `scores` view still return issue occurrences (source `issue`), and the web scores tab hides them. The trace and session views show issue chips instead.

### D-IC-12 Runs, jobs and limits

- **Queue.** `issue_candidates` is its own queue, separate from `eval_jobs`, which needs an evaluator. It has I's job columns: `status`, `attempts`, `claims`, `interruptions`, `load_retries`, `not_before`, `error_class`. It also has I's crash rules: startup re-queues `running` rows, and a candidate interrupted 3 times fails (D-I-12).
- **Workers.** A small IC worker pool (`issues.workers`, default 4, 1–64) claims candidates. Each claim takes a slot of the connection's concurrency and a token of its bucket from I's per-connection limiter (D-I-6), shared with judges. A candidate waits while judges hold the connection's slots; there is no strict priority across the two queues, and the daily caps bound IC's share. Error fingerprinting and assignment run in the sweeper and in the worker, without a queue.
- **Retries and pauses.** They follow D-I-13 and D-I-14: a pause-class error pauses the source, and a test of the connection resumes it.
- **Budget.** It follows D-I-15 on the summed cost of the three call types. The estimate for a summary is `ceil(chars / 4)` input tokens plus 200 output tokens, and for an embedding the summary's `ceil(chars / 4)`. A refit's naming calls count as well.
- **Run history.** Each refit is an `issue_refits` row: start, end, pending count, clusters, issues created, candidates joined to existing issues, cost, and error class (no provider text, D-I-24's rule). Each candidate keeps its status and error class. The last 50 refits per source are kept.

### D-IC-13 Plan gating

`max_active_issue_sources` becomes P's tenth limit (D-P-13; config key `entitlements.defaults.max_active_issue_sources`, the org token's limits claim gets the same key; P's catalog gives Free 1 and paid plans unlimited, `None`). It counts enabled sources of active projects in the org, paused or not, of every kind (as D-I-32 counts rules). The check is the same as D-K-14's and D-I-32's: `403 plan_limit` on create or enable. Everything stays MIT (as D-I-33): Free can use it, and the customer's key pays for the calls.

### D-IC-14 Links

- **Alerts** (track K):
  - **Condition.** See D-IC-20.
  - **Notifications.** See D-IC-21.
- **Datasets** (track J): "Add examples to dataset" on an issue adds up to 100 of its most recent occurrence traces through J's bulk-add code, with metadata `cetra.issue_id` (D-IC-22).
- **Annotation queues** (track AQ): "Send to queue" on an issue adds its newest occurrences to a queue through AQ's add-by-id path (D-IC-27 to D-IC-31).
- **Trace view**: an Issues chip row on a trace or session links to each issue, and the trace list gets an "Issue" facet.
- **MCP** (D-IC-24): a read-only `list_issues` tool (open issues of a project with counts) joins H's tools.

### D-IC-15 Crate boundaries

- **`cetra-core`.** `ScoreSource::Issue`, which is a shared contract and is requested from the coordinator in M1's plan. Issue types stay in `cetra-server`, as evaluators do.
- **`cetra-eval`.**
  - `Embedder`: an embeddings call for the OpenAI-compatible (`/embeddings`) and Gemini (`embedContent`) adapters, with an `embedding` span in the call's trace;
  - a generic structured completion that takes any JSON schema, so the judge path and the summary and naming calls share one adapter path; the judge keeps its `submit_judgement` tool name;
  - the error-message normalization and the fingerprint, as pure functions;
  - the cosine, running-mean and average-linkage clustering, as pure functions. They are kept here so the server crate holds no numeric code, and they are tested with property tests.
- **`cetra-storage`.** `error_spans` (M1, D-IC-2), and a name-list form of the score reads for the issue counts (M1) and `AlertMeasure::ScoreCount` (M4, D-IC-20).
- **`cetra-server`.**
  - an `issues/` module with the tables, sources, sweeper hooks, worker job kinds, the refit loop, the routes, and the deletion and pruning hooks;
  - the K condition and notifications, in `alerts/`.
- **`web/`.** The Issues pages, the source editor and the chips and facet.

No crate dependency changes: `eval` still depends on `core` and `otel` only.

### D-IC-16 Pages

- **Issues** (sidebar Monitor, next to Alerts, `g` then `y`):
  - a list with state tabs (Open, Resolved, Ignored, All), the source and kind filter, sort by last seen, count over the period or first seen, and a sparkline per row;
  - an issue page with the title (editable), the description, the state actions, the regressed badge, the daily occurrence chart, affected users, first and last seen, example traces (the newest 20 with their summaries), events, "View traces" (the trace list filtered), "Add examples to dataset", "Create alert" (a prefilled K rule) and "Merge into…".
- **Issue sources** (sidebar Evaluate, next to Online rules) work like the online rule pages:
  - the list shows the state, the 24 h candidates, the spend against the budget and the last refit;
  - the editor holds the kind, the filter with preview, the sampling, the cap, the score names, the connections and models (embedding fixed after create), the floor and minimum size, the budget and the notification channels;
  - a source page holds the refit history and a "Refit now" button.

### D-IC-17 Exact contracts of the pure functions and calls

These are fixed here so the plans copy them, not choose them.
- **Fingerprint bytes.** SHA-256 over the label `cetra.issue-fp.v1`, then for each of the four fields (`service_name`, span name, exception type, normalized message), a `u32` big-endian byte length and the UTF-8 bytes. An absent field has length 0. The first 16 bytes, as lowercase hex, are the stored fingerprint.
- **Normalization.** The rules run in the order D-IC-2 lists them. Each replaces a whole match with a placeholder: `<url:SCHEME://HOST>` (the URL's lowercased scheme and host kept), `<email>`, `<uuid>`, `<time>`, `<ip>`, `<path>`, `<hex>`, `<str>`, `<n>`. A later rule never rewrites inside an earlier placeholder. The M2 plan gives the exact regular expressions.
- **Clustering determinism.** Candidates enter the refit ordered by (`created_at`, id). The nearest-neighbour chain breaks distance ties by the lower index, and a cluster's members keep the input order. The same input always gives the same clusters.
- **Vectors.** `cetra-eval` L2-normalizes every embedding it returns, so cosine similarity is a dot product. It refuses an answer of more than 4 096 dimensions or with a non-finite value (`EmbeddingInvalid`, permanent). The server checks the dimension against the source's (`embedding_dimension_changed`).
- **Call traces.** A summary, embedding or naming call writes one trace in environment `cetra-eval`: a root `chain` span named `cetra.issue.summarize`, `cetra.issue.embed` or `cetra.issue.name`, with one `llm` or `embedding` child, built the way judge traces are (D161) and priced by `normalize`. A provider that reports no token count leaves the span unpriced. The budget then charges the estimate of D-IC-12.

### D-IC-18 Occurrence ids, hiding and counts

- **An occurrence score id is a UUIDv7** whose 48-bit millisecond timestamp is the occurrence time and whose other bits come from SHA-256(`cetra.issue-occurrence.v1` ‖ project id (16 bytes) ‖ trace id (16) ‖ issue id (16)), with the version and variant bits set. `scores::write` then takes `created_at` from the id (the D-G-26 path) and a rewrite of the same (trace, issue) replaces the same row.
- **The occurrence time of an error issue is the `start_time` of the trace's earliest error span with that fingerprint**, clamped to `now`. It is stable across re-sweeps. When a late span of the same fingerprint starts earlier still, a second row is written for the trace, and the count is one too high (accepted, rare). Occurrences older than the project's retention are not written (`scores::write` refuses them). A span that starts in the future is clamped to the sweep time, so a re-sweep of it, or of a trace after a merge, can add a second occurrence; accepted.
- **Occurrences carry the trace's user and session**: from the first error span of the group (in `(start_time, span_id)` order) with a non-null `user_id`, and likewise for `session_id`.
- **`latest_scores` and the users' `scores_of` leave out source `issue`.** The list columns and briefs never see issue scores. The trace view gets its chips from `GET …/scores?trace_id=…&source=issue`, and the scores tab hides rows of source `issue`. The dashboard scores source leaves out source `issue` too.
- **No `occurrence_count` column.** Counts come from storage (`score_name_counts`), so deletions are reflected.
- **`issue_sources.kind` has no SQL CHECK**, so M3 adds kinds without rebuilding the table. The Rust enum is the only gate.
- **Regression compares trace times.** A trace that started before `resolved_at` but completes after it does not reopen the issue.

### D-IC-19 M3 build decisions

- **Prompt inputs.** The summary prompt uses the root span's full `input` and `output`, rendered to text and cut to 4 000 characters each (the `*_preview` columns hold only 512). The topic prompt uses the first user message: when the root input is in `messages` format, the text parts of its first `role: "user"` message, joined with a newline; otherwise the root input text; cut to 4 000 characters.
- **The summary is kept across retries.** A candidate's summary is stored as soon as it is made, so a failed embedding retries without a second summary call.
- **Paused detail.** `issue_sources.paused_detail` holds the provider's error text for a connection pause, shown on the source page like a rule's.
- **Refit schedule.** `issue_sources.next_refit_at` (ns) is creation + 1 h, then the end of each refit + 6 h. "Refit now" sets it to now and is refused with 409 when a refit started less than 10 minutes ago.
- **Naming waits for a slot.** A refit takes a chat-connection slot for each naming call, retrying every second for up to 2 minutes. If none comes, the refit stops with error class `busy`, and the issues it already created stay.
- **Embedding connection test.** `POST …/llm-connections/{id}/test` takes an optional `embedding_model`. When it is set, the test embeds `"ping"` instead of asking for a judgement. A success resumes the sources paused for that connection.

### D-IC-20 The issue_occurrences condition
- K gets a fifth metric `issue_occurrences` (amends D-K-2 "four conditions" and spec 07). `AlertCondition` gains two optional fields `issue_id` and `source_id` (UUID strings). Exactly one of them is set for `issue_occurrences`; both must be absent for the other metrics (D-K-2's "fields that do not apply must be absent").
- For `issue_occurrences`: `filter`, `min_count`, `target`, `score_name`, `aggregation` are refused ("does not apply"). `threshold` is finite and >= 0. The value is the count; zero is a value (like `cost`), never "no data".
- The window of an `issue_occurrences` rule is `15m` or more: `1m` and `5m` are refused (400 `condition.window`: "issue occurrences need a window of 15m or more"). An occurrence is written after its trace completes (a rooted trace idle 5 s, else the 60 s timeout, then the sweep), later than its `created_at` (the trace's start), so a window of a few minutes would miss occurrences written after the window moved on. Error issues are near-real-time. LLM occurrences are written when a candidate joins (minutes after its score or trace) or by a refit (every 6 hours), dated at the trace start, so a refit can add occurrences up to about 6 hours in the past: docs advise a `6h` or `24h` window for LLM sources.
- `checked_condition` checks the issue or source exists in the rule's project (400 `condition.issue_id` / `condition.source_id`: "no such issue in this project" / "no such issue source in this project").
- Storage: no new function; the existing `alert_value` gets a new measure. `cetra_storage::AlertMeasure` gets a variant `ScoreCount { names: Vec<String> }`: the number of live scores whose name is in `names` and whose `created_at` is in the window. No trace filter, no default exclusions, any target. At most `MAX_COUNT_NAMES` (1 000) names, else `InvalidRequest`. `AlertValue { count: n, value: Some(n as f64) }`.
- Names at each evaluation (server, `alerts/evaluate.rs` before building the request):
  - issue scope: resolve the issue; if it is merged, use its `merged_into` target (merges are retargeted, so one hop). Names = the target and every issue merged into it (`issue:<id>` each).
  - source scope: every issue of the source whose state is not `ignored` or `merged`, plus the issues merged into each.
  - The names are split into chunks of `MAX_COUNT_NAMES`, one `alert_value` each, counts summed (no cap on names).
- If the issue or the source no longer exists (source deleted), the evaluation fails with the fixed reason `issue not found` / `issue source not found`, handled like any failed measure read (K's existing path). Deleting a source does not touch alert rules.
- A rule on `issue_occurrences` counts toward D-K-14's rule limit like any rule.

### D-IC-21 Issue notifications
- Each issue source has one optional list of notification channels (K's project channels), used for both **new** and **regressed** issues: table `issue_source_channels(source_id, channel_id)`, both FKs `ON DELETE CASCADE`, primary key on the pair, index on `channel_id`. At most 10 channels per source, validated like `checked_channels` (owned by the project, no duplicates). Source create/update requests and responses get `channel_ids`. Permission: the source routes' (`ManageEvaluation`).
- `issue_sources.last_notified_at INTEGER` (nullable). Enabling a source (create enabled, or disabled → enabled) sets it to now, so a backlog is never sent. The same when a source's channel list goes from empty to non-empty (a create with channels counts only if the source is enabled; an update): `last_notified_at` is set to now. Changing a non-empty list to another non-empty list, or to empty, does not touch it.
- Replaces D-IC-14's `issue_event_id` column: a new table `issue_notifications(id TEXT PK, source_id TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE, digest TEXT NOT NULL, created_at INTEGER NOT NULL)`. `digest` is JSON: `{"items":[{"issue_id","kind":"created"|"regressed","title"}], "more": n}` (at most 20 items, newest first). `notification_deliveries` is rebuilt (SQLite cannot drop NOT NULL): `event_id` nullable, new nullable `issue_notification_id REFERENCES issue_notifications(id) ON DELETE CASCADE`, `CHECK ((event_id IS NULL) <> (issue_notification_id IS NULL))`, the three existing indexes kept plus one on `issue_notification_id`.
- The tick: the alert evaluation loop (each tick) runs `notify_issues(state, now)`. For each enabled source of an active project in an active org with at least one channel and `last_notified_at <= now - 10 min`: collect `issue_events` of its issues with kind `created` or `regressed` and `last_notified_at < at <= now - ISSUE_NOTIFY_SETTLE_NS` (a 60 s settle margin, so an event whose transaction committed a little after its `at` is not lost; `created` and `regressed` events are stamped with the clock read inside the transaction that inserts them, never with a time read before the write, so an event is never older than the cut-off of a notification sent while its work was running). None → nothing. Else, in one transaction: insert one `issue_notifications` row (digest of up to 20, newest first, `more` = the rest), one pending delivery per channel, and set `last_notified_at` = the newest event's `at`. Events at or before `last_notified_at` are never sent. The cut-off is the newest collected event's `at`, which is up to the settle margin (60 s) older than the moment of the send, so two digests of one source are at least about 9 to 10 minutes apart (10 minutes from the first cut-off, not from the first send). Events of an issue that is ignored or merged by the time of the tick are still sent (accepted). A paused source still notifies; a disabled source does not.
- Delivery: `due_deliveries` returns both kinds (rule deliveries as today; issue deliveries joined through `issue_notifications` → `issue_sources` → projects → orgs). Retries, backoff, `MAX_ATTEMPTS`, the delivery header and pruning are K's. `issue_notifications` are pruned like `alert_events` (90 days, cascading their deliveries).
- Messages: webhook kind `issues.detected` with `{source_id, source_name, project, items, more, issues_url}`; Slack: a header "N new or regressed issues in <source>" ("1 new or regressed issue in <source>" for one), up to 20 lines (kind word + title linked to the issue page), "and N more", a button "View issues" → `/<org>/<project>/issues?source=<id>`; email: a new `Email::Issues` variant and `issue_notification.{html,txt}` templates, subject `"[<PRODUCT>] <N> new or regressed issues: <source name>"` (`issue` for N = 1, in the subject and the body). Email quota (D-K-19) applies as for alerts. The channel test-send is unchanged (alert sample).
- Titles go to the user's own channels (as rule names do); logs carry ids and counts only.

### D-IC-22 Add examples to a dataset
- Route `POST /api/v1/projects/{project}/issues/{issue}/dataset-items`, body `{"dataset": "<id or name>", "limit": 1..=100 (default 100)}`, permission `ManageDatasets`. Answer: J's `FromTracesResult` (201).
- The server picks the most recent occurrences by `created_at` of the issue and up to 7 merged issues (the 8 names the occurrences examples use: the merged issues with the most occurrences over the default occurrences window, the last 14 days ending now), one per trace (distinct traces), up to `limit`, any issue state. Each item uses the occurrence's span (`cetra.issue.span_id` in the score metadata) when present, else the trace root.
- It reuses J's bulk-add code: the core of `add_from_traces` is extracted into a `pub(crate)` function taking the trace refs and a metadata map; the public J route passes an empty map, so J's API and D-J-25 are unchanged. Items get metadata `{"cetra.issue_id": "<issue id>"}` (namespaced like other `cetra.*` keys; amends D-IC-14's `metadata.issue_id`).
- Web: "Add examples to dataset" on the issue page (members with `canManageDatasets`), a dialog with J's dataset picker (search + new dataset) and the result view with skipped traces, as `add-traces-to-dataset-dialog.tsx` does.

### D-IC-23 Create alert from an issue
- The issue page gets "Create alert": it opens `alerts/new` with search params `metric=issue_occurrences&issue_id=<id>`, and `RuleForm` takes initial values (prefill only; the user saves). Defaults: window 1h, comparator gt, threshold 0.
- The rule form's metric picker gets "Issue occurrences" with an issue-or-source scope picker.

### D-IC-24 MCP list_issues
- A tenth read-only tool `list_issues` (amends D-H-11, after the prompt tools of D-V-39). Arguments: `project` (required), `state` (`open` default, `resolved`, `ignored`, `all`), `source` (id, optional), `limit` (1–100, default 20), `max_chars`. Output `{issues: [{id, title, kind, state, regressed, source_id, first_seen, last_seen, count_7d, users_7d, url}], truncated, notes}`, sorted by last_seen desc. Counts over the last 7 days through the same tally helper as `GET /issues` (moved from `routes/issues.rs` to `issues/` as `pub(crate)`). `ProjectPerm::View`.

### D-IC-25 End-to-end coverage
- The CLI e2e (`crates/cli/tests/e2e.rs`) covers error issues with the real binary: an error trace creates an open issue with the trace as an example, resolve, a second trace regresses it, then "add examples to dataset" copies it. It does not wait for alerts (60 s tick).
- Alerts on `issue_occurrences` and issue notifications are covered in-process with `AlertTiming::TEST`, a `HookReceiver` and the manual clock (the 10-minute throttle).

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

## Architecture

```
ingest ─► PendingTraces (I) ─► sweeper ─┬─ online rules (I)
                                        ├─ error sources ─► fingerprint ─► issue (create/join) ─► occurrence score
                                        └─ topic sources ─► candidate (queued)
scores::write ─► score/feedback sources ─► candidate (queued)
candidate ─► dispatcher + connection limiter (I) ─► summarize ─► embed ─► assign ─┬─ join issue ─► occurrence score
                                                                                   └─ pending ─► refit (6 h / on demand)
refit ─► cluster pending ─► join existing / name new issue (LLM) ─► occurrence scores
issue events (created, regressed) ─► K channels       occurrence scores ─► K `issue_occurrences`, dashboards, trace filter
```

### Tables (`meta.db`, end state; exact DDL in the plans)

| Table | Holds |
|---|---|
| `issue_sources` | id, project, name, kind, filter, sample_rate, daily_cap, score_names, feedback_sources, chat connection and model, embedding connection, model and dimension, floor, min_issue_size, budget, enabled, paused and reason, last_notified_at, created_by, timestamps |
| `issues` | id (UUIDv7), project, source, fingerprint (error kind, unique per source), title, description, state, regressed, merged_into, centroid BLOB, member_count, first_seen, last_seen, occurrence_count, resolved_at and by, timestamps |
| `issue_source_channels` | source, channel (the source's notification channels, D-IC-21) |
| `issue_notifications` | id, source, digest (JSON: up to 20 items, newest first, and `more`), created_at |
| `issue_source_days` | source, UTC day, candidates accepted, USD spent (the daily cap and the budget, as `rule_spend`) |
| `issue_candidates` | id, project, source, trace_id, span_id, session_id, user_id, trace_start, score_id, status (`queued`, `running`, `pending`, `assigned`, `outlier`, `dropped`, `failed`, `skipped`), the queue columns of D-IC-12, issue, summary, summary_prompt_version, vector BLOB, model, dimension, call trace id, cost, error class, timestamps; unique on `dedup_key` (`<source>:<trace>`, `:<span>` or `:session:<id>`) |
| `issue_events` | id, issue, kind (`created`, `state_changed`, `renamed`, `merged`, `regressed`), actor, from and to, at |
| `issue_refits` | id, source, started and finished, counts, cost, error class |

Deletion (track M):
- A trace, user or project deletion deletes the matching `issue_candidates` rows, as track J's cascade does for dataset items copied from traces (D24). The occurrence scores go with the scores.
- An issue keeps its title and description after its members are deleted (D-IC-26).
- Dropping a project or org cascades by foreign key.

### API (summary; the plans give the exact types)

| Route | Use |
|---|---|
| `GET/POST /projects/{p}/issue-sources`, `GET/PATCH/DELETE …/{id}` | sources; `PATCH` refuses an embedding change |
| `POST …/issue-sources/preview` | the filter's match count over 24 h and the cost estimate at the sampling and cap |
| `POST …/issue-sources/{id}/refit`, `GET …/{id}/refits` | on-demand refit (`202`), history |
| `GET /projects/{p}/issues` | list: state, source, kind, `q` on title, sort, keyset paging, with counts for a period |
| `GET/PATCH …/issues/{id}` | detail; `PATCH` sets title, description or state |
| `POST …/issues/{id}/merge` | `{into}` |
| `GET …/issues/{id}/occurrences` | daily counts and the newest examples |
| `GET …/issues/{id}/events` | history |
| `POST …/issues/{id}/dataset-items` | `{dataset, limit ≤ 100}` |

Permissions follow online rules (D-I-24): members manage sources, viewers read issues, and members change states.

### Logs and privacy

- Logs carry ids, counts, sizes and error classes, never summaries, titles, prompts or vectors.
- Summaries, titles and descriptions are payload-derived and live only in `meta.db` and the scores' `reason`, under the same rules as dataset items and judge reasons.

## Milestones

Each milestone ends in working, tested behaviour, with its spec sections, `CHANGELOG.md` line and docs-site page updated in the same PR.

| # | Delivers | Depends on | Leaves out |
|---|---|---|---|
| **M1** Error issues | The `error_spans` storage read, sources (kind `error` only), the `issues` and `issue_events` tables, fingerprinting on completed traces through I's sweeper, occurrence scores (`ScoreSource::Issue`), the lifecycle (resolve, ignore, reopen, regression) and merge, the issues and sources API, the Issues list and page, the source editor for errors, the trace chips, the facet and filter, the `max_active_issue_sources` limit | I, K, P, M (all done); the coordinator's `ScoreSource::Issue` | Every LLM kind, refit, embeddings, alerts |
| **M2** Provider calls | `cetra-eval`: the embedding call (OpenAI-compatible, Gemini) with its traced span and price, the generic structured completion, the clustering math with property tests; an embedding model test on the connection test route | E, I (done) | Server use of them |
| **M3** LLM sources | Kinds `score`, `feedback`, `topic`: the score-write and completion triggers, candidates, summarize and embed jobs on I's dispatcher, the budget and pauses, assignment, the refit with naming, the refit history and "Refit now", candidate pruning, the deletion cascade for candidates, the editor for LLM kinds | M1, M2 | Alerts, datasets |
| **M4** Links | `AlertMeasure::ScoreCount` in storage, K's `issue_occurrences` condition, the `notification_deliveries` change and the new or regressed notifications, "Add examples to dataset", MCP `list_issues`, the end-to-end test | M3; K, J, H (done) | Annotation queues |
| **M5** Queues | "Send to queue" on the issue page: `POST …/issues/{id}/queue-items`, the shared AQ add core, the dialog | M4; AQ (done) | Bulk send from the list, a count field in the dialog, a record of the issue on the item |

M1 and M2 can run in parallel: M1 touches `cetra-core`, `cetra-storage`, `cetra-server` and `web/`, and M2 only `cetra-eval`; they share `CHANGELOG.md` and the decision log, merged by hand. There is no backfill: a source sees traces and scores from its creation on (a non-goal of the MVP; I's backfill is the model if users ask). M5 has its own plan (`05-m5-queues.md`).

## Decided questions

The user answered the design's open questions on 2026-11-20 (D-IC-26). Each answer keeps the design as built.

1. **Units.** Occurrence scores are not billable units (D-IC-11).
2. **Deletion and titles.** An issue keeps its LLM-written title and description after all of its members are deleted.
3. **Regression rule.** A regression reopens the resolved issue, Sentry style; it does not create a new one.
4. **Defaults.** Daily cap 500, floor 0.80, minimum issue size 5, a refit every 6 h on at least 20 pending candidates. Compared with the open-source competitors on 2026-11-20: the floor sits between Opik's fixed 0.75 and the p95-radius rules of Langfuse and LangWatch; size 5 equals LangWatch's minimum (Langfuse uses 15); the refit is the most frequent and needs the fewest items (Langfuse refits daily on 100 or more summaries, LangWatch every 2 to 7 days on more than 100 unassigned traces). The user kept 6 h and 20, because candidates join existing issues continuously and a refit only creates issues from the remainder.
5. **Topic issues.** Topics share the Issues page, filtered by kind.
6. **Digest counts.** A notification digest counts events, not distinct issues: an issue created and then regressed in one window counts twice, and events past the first 20 are counted in "and N more" but never sent later (D-IC-21).
