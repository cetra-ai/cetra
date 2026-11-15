# 06 — Evaluation: scores, evaluators, online rules, datasets, experiments

## Scores

Data model in 01; written through:

| Path | `source` |
|---|---|
| `POST /api/v1/scores` (batch ≤ 1000) with `sk` key (`ingest` scope) | `sdk` (D-F-1) |
| `POST /api/v1/projects/{p}/scores` (batch ≤ 1000) with `sk` key or PAT | `api` (or `sdk` when the SDK sets it) |
| UI annotation form on traces, spans and sessions (one control per live score definition; only the caller's own scores, D-I-37) | `human`, `author_user_id` set |
| `POST /api/public/feedback` with `pk` key | `end_user` |
| Online rules / experiment server evaluators | `llm_judge` \| `check` \| `webhook` |

Evaluator scores (`llm_judge`, `check`, `webhook`) are written by the eval queue with `evaluator_id` set, and `rule_id` set for rule jobs; their derived id's author is the rule, or the evaluator for a manual run (D-I-18).

Scores go through a WAL + memtable exactly like spans (03), so they are durable on `200` and visible in seconds. Update = write a new row with the same `score_id` and later `updated_at`; delete = row with `deleted = true`. Every version keeps the first version's `created_at` (03 §5): a UUIDv7 `score_id` takes it from the id's time, any other id from the live version, and a new id from the write time (D-G-26).

The caller decides `source` (D-G-27): a session on the project route writes `human` (with `author_user_id`); a PAT or `sk` key there writes `api`, or `sdk` when the body says so; `POST /api/v1/scores` writes `sdk`; feedback writes `end_user`. Any other `source` in a request is a `400`.

**Ownership** (D-I-26, D-I-36). A `human` score belongs to its `author_user_id`. Through a session or a personal access token, a write whose `score_id` names the live `human` score of another user, or a `DELETE` of one, is `403 forbidden` unless the caller is a project admin. Validation errors (`400`) and the units cap (`402`) come first. An admin who replaces another user's score becomes its author. A member may replace another user's non-`human` score (`api`, `sdk`, `end_user` or an evaluator's) by its id, deliberately: the stored version becomes their own `human` score (D-I-26). `sk` keys, public feedback and evaluators keep the rules above: they can replace a `human` score whose id they know. A person's write without `score_id` derives the id from their own user, so it never replaces anyone else's score. A person's write that supplies UUIDv7 ids looks them up in storage first, so it can answer `503` when storage is unavailable. The check and the append are not atomic; a concurrent race is accepted.

**Deterministic score IDs** (one row per annotator, as Opik's `authored_feedback_scores`): when the client does not provide `score_id`, the server derives it as a UUIDv8 (the first 16 bytes of SHA-256 over the ASCII tag `llmobs.score.v1` followed by the project id, target kind, trace id, span id or session id, name, source and author, each as a big-endian `u32` byte length then its UTF-8 bytes; D-G-28) where `author` = `author_user_id` for `human`, `rule_id` for online judges, `experiment_id + dataset_item_id` for experiments, the key or PAT id for `api`/`sdk`/`end_user`. The SDKs always send a `score_id` (a UUIDv7 per `score()` call, D-F-2), so this applies to other clients that omit it. Consequences: re-annotating replaces your own score instead of adding a duplicate; two reviewers scoring the same trace keep two rows; a backfill re-run overwrites instead of duplicating. Aggregations (`score()` in the DSL, dashboards) average across authors.

Validation: `data_type` ∈ {numeric, boolean, categorical, text}; numeric must be finite; boolean must be 0/1/true/false; categorical requires `value_str`; target must have the matching ID fields; `name`, a session target's id and a categorical label are 1–255 bytes. A batch is all-or-nothing. If a score definition with the same name exists, values are validated against it (range, categories) — otherwise accepted as is. Only live (non-archived) definitions count; definitions are cached 10 s per project (D-I-26).

**End-user feedback** (`pk` key, browser-safe): body `{trace_id, span_id?, name? = "user_feedback", value (number|boolean|string), comment?}`. A number is `numeric`, a boolean `boolean`, a string a `categorical` label; `comment` is the `reason`; the id is derived with the key as author, so a second answer for the same trace and name replaces the first. CORS allowed from any origin; rate limit 10 req/s per key+IP; payload ≤ 4 KiB. The trace is not required to exist yet (feedback can arrive before the trace is flushed).

## Evaluators

Stored in SQLite `evaluators` (04). Three kinds.

**Test runs** (D-I-21): `POST /evaluators/test` runs a saved or unsaved evaluator once on a stored trace with a mapping, or on literal variables. It writes the judge trace and the judge usage, writes no score, and answers the outcome, the resolved variables, the rendered prompt and the usage; 10 per minute per user.

### `llm_judge`

```json
{
  "connection_id": "…", "model": "gpt-5-mini",
  "prompt": "You are grading … Question: {{ input }}\nAnswer: {{ output }}\n…",
  "output": {"type": "numeric", "min": 0, "max": 1}
          | {"type": "boolean"}
          | {"type": "categorical", "labels": ["correct", "partially_correct", "incorrect"],
             "label_values": {"correct": 1, "partially_correct": 0.5, "incorrect": 0}},
  "include_reason": true,
  // temperature is optional and unset by default
  "max_output_tokens": 1024,
  "timeout_ms": 60000
}
```

- Prompt templating: `minijinja` with the variables of the mapping (below); missing variables are an error at job time (`failed: missing variable x`).
- Structured output is a per-connection setting, `structured_output`: `native` (default), `tool` or `prompt` (D157). `native` is OpenAI-compatible `response_format: {type: "json_schema", strict: true}`, Anthropic `output_config.format` with a JSON schema, and Gemini `responseMimeType: "application/json"` with `responseJsonSchema`. `tool` is a forced tool or function call carrying the same schema; it is not the default, because newer Claude models refuse a forced `tool_choice`. `prompt` appends an instruction and the schema to the prompt and parses the text. There is no automatic downgrade and no cached capability state. A provider that rejects the mode returns a permanent error that names the setting to change.
- The schema the model sees is an object with `explanation` (string, present only when `include_reason`) first, then `value` (number or boolean) or `label` (an enum of the labels) (D158). `additionalProperties` is false, every property is required, and there is no `minimum` or `maximum`; ranges are checked after parsing. The same parser reads every mode. It accepts a tool call's arguments, text that is one JSON object, text in a markdown fence, or the first complete JSON object in the text. It coerces `"0.8"` to 0.8 and `"true"` / `"false"` to booleans, joins an array explanation with newlines, and matches labels after trimming and case-folding. Unknown labels, out-of-range numbers, missing fields, an empty answer and a refusal are typed errors. The explanation becomes the score's `reason`. The field the model sees is never named `reasoning`.
- Requests (D159): the OpenAI-compatible adapter uses Chat Completions (`POST {base_url}/chat/completions`, default base `https://api.openai.com/v1`) with `max_completion_tokens`. Anthropic uses Messages with `max_tokens`, which is required. Gemini uses `models/{model}:generateContent` with `maxOutputTokens`. The prompt is one user message. `temperature` is optional and unset by default; the built-in templates do not set it. `max_output_tokens` defaults to 1024. Timeouts are 10 s to connect and 60 s for the whole call (`timeout_ms` on the evaluator, at most 300 s). A response over 4 MiB is an error.
- Result → score: numeric `value_num`; boolean 0/1; categorical `value_str` = label and `value_num` = `label_values[label]` when provided; `reason`.
- **Judge traces** (same pattern as Langfuse, Opik, Phoenix): every evaluator execution (judge, check or webhook, success or failure) is traced into the **user's project** through the internal ingestion path (spans built in-process and handed to the WAL, no HTTP), with:
  - reserved `environment = "llmobs-eval"`;
  - a root `evaluator` span named after the evaluator; a judge adds one `llm` span (full prompt, response, tokens, cost); a webhook adds one child of kind `span` named `webhook` (there is no `http` span kind);
  - metadata `llmobs.eval.target_trace_id`, `llmobs.eval.target_span_id`, `llmobs.eval.rule_id` | `llmobs.eval.experiment_id`, `llmobs.eval.evaluator_id`.
- The trace is built as an OTLP `ExportTraceServiceRequest` and passed through `llmobs_otel::normalize` with the caller's ingest context and price table, so previews, truncation, media extraction and cost match SDK spans (D161). The caller supplies the trace id (the job's, so a retry reuses it). Span ids are the first 8 bytes of SHA-256(trace id ‖ span index). `judge.cost_usd` is the `llm` span's `cost_total`; when the judge model has no price, the outcome says so.
- The score links back: `metadata['judge.trace_id']`, plus the usage summary `judge.model`, `judge.input_tokens`, `judge.output_tokens`, `judge.cost_usd`, `judge.latency_ms` (so score lists show cost without opening the trace). `eval_jobs.cost_usd` and `usage_daily.judge_*` are updated too; judge-trace spans are not billable units (D-I-31, D178).
- **Isolation of `llmobs-eval` traces:** never selected by online rules (no recursion); excluded by default from trace/span/session lists, user aggregates, the dashboard and alert queries (05, 07); visible with the "Show evaluation traces" toggle or an explicit `environment = 'llmobs-eval'` filter; the dashboard has a separate "Evaluation cost" series. The environment name is reserved: ingestion from external sources rewrites it to `llmobs-eval-external` with a warning attribute. The OTLP handler does the rewrite after normalization; evaluation spans are built in process and keep the name.
- UI: "View judge trace" link on every evaluator-produced score; the evaluator test panel shows the same trace inline.

### `check` (built-in, pure Rust)

| `check` | Params | Score |
|---|---|---|
| `exact_match` | `case_sensitive`, `trim` | boolean |
| `contains` | `value` or `values[]`, `mode: any\|all`, `case_sensitive` | boolean |
| `regex` | `pattern` | boolean (match found) |
| `json_valid` | — | boolean |
| `json_schema` | `schema` | boolean, reason = first validation error |
| `levenshtein` | `normalize: true` | numeric 0–1 similarity |

Checks compare `output` against `expected` (or the param value).

**Defaults** (D164). Text extraction: a string is used as is; a messages array yields the concatenated text parts of its last assistant message, or empty if there is none; any other JSON yields compact JSON; an absent value is an error. `exact_match` defaults to `case_sensitive` true and `trim` true. `contains` defaults to `case_sensitive` false and `mode` any; omitting both `value` and `values` compares the output with `expected`, and an empty needle string is invalid configuration. `regex` uses Rust regex syntax, searches anywhere, and rejects a pattern whose compiled size exceeds 1 MiB or that does not compile. `json_valid` requires the whole extracted text to parse as JSON; an output that is already a JSON object or array is valid. `json_schema` is Draft 2020-12; on failure the reason is the first error's instance path and keyword (for example `/answer: type`) and never the instance value. `levenshtein` is `1 − distance / max(len)` over Unicode scalar values, and 1.0 when both strings are empty; `normalize` defaults to true and then trims, case-folds and collapses whitespace runs on both sides before the distance.

### `webhook`

```json
{"url": "https://…", "timeout_ms": 10000, "headers": {…}}
```

The URL, the headers and the signing secret are sealed in `evaluators.secret_enc`; the stored config and API responses show only `url_host`, `timeout_ms` and the header names (D-I-19).

Request: `POST url` with `Content-Type: application/json`, the configured headers, and JSON `{evaluator: {id, name}, project_id, trace_id, span_id, variables: {…}}`. The signature header is `X-Llmobs-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, t + "." + body)>` (D165). `timeout_ms` defaults to 10 000 and is at most 60 000. Redirects are not followed. A response body over 64 KiB is an error. A `200` whose JSON object `{value?: number|boolean, label?: string, reason?: string}` fits the output definition is a score. Status classes: 408, 425, 429 and 5xx retry; 401, 402, 403, 404 and 410 pause; every other status is permanent (D160, D165). A `200` with a body that is not that object is a permanent error. A categorical webhook output needs a non-empty `categories` list on its output score, with each label checked as a judge label is; the API refuses one without it (field `output_score.categories`).

## Variable mapping

Online rules and experiment evaluators map template variables to data:

| Source expression | Meaning |
|---|---|
| `trace.input` / `trace.output` | input / output of the trace root: the span with no parent, or the earliest such span if several; if there is no root, the first `llm` span |
| `span.input` / `span.output` | the target span (span rules) |
| `span[name="retrieve"].output` | first span with that name, by start time |
| `span[kind="retriever"].output` | first span with that kind, by start time |
| `trace.metadata.<key>`, `span.metadata.<key>`, `span.attributes.<key>` | metadata / attribute values |
| `trace.transcript` | compact rendering of the whole trace, for agent-trajectory judges (rendering below) |
| `expected` | dataset item `expected_output` (experiments only; absent without a dataset item) |
| `item.input`, `item.metadata.<k>` | dataset item fields (experiments only; absent without a dataset item) |

Any expression may end with an RFC 9535 JSONPath suffix (`trace.output$.parts[0].content`, through `serde_json_path`) and an optional trailing `?` that marks the variable optional (D162). Payloads decode by their format (`messages` and `json` as JSON, `text` as a string); metadata and attribute values are JSON-decoded. A JSONPath with no match is absent, one match is the value, and several matches are an array. An absent required variable is `missing variable <name>`. An absent optional variable renders as an empty string.

**Rendering** (D163). Templates use `minijinja` with strict undefined (a missing variable is an error, never literal text), no auto-escaping, a fuel limit of 100 000 and a template limit of 64 KiB. In a prompt, a string renders as itself, null as an empty string, a messages array as one `role: text` line per message (non-text parts as `[image]`, `[audio]` or `[file]`, tool calls as `tool_call name(arguments)`), and any other JSON as compact JSON. Templates may index into JSON values (`{{ output.answer }}`). `trace.transcript` renders a tree of `kind name: input → output` lines, two spaces of indent per level, children in start-time order. Each payload is cut to 2 000 characters with `…`, and the whole rendering is cut to 60 000 characters with a final `[… n more spans]` line. Lengths are Unicode scalar values.

## Built-in judge templates

Shipped as code in `llmobs-eval`; "create from template" copies the prompt into a new evaluator the user can edit.

| Template | Variables | Output |
|---|---|---|
| Hallucination | `input`, `output`, `context` (default mapping: `span[kind="retriever"].output?`) | boolean (true = hallucinated) |
| Relevance | `input`, `output` | numeric 0–1 |
| Toxicity | `output` | boolean |
| Correctness | `input`, `output`, `expected` | categorical correct / partially_correct / incorrect (1 / 0.5 / 0) |
| Custom | user-defined | user-defined |

## Online rules

`online_rules` (04): `{evaluator, target: trace|span, filter (DSL), variable_mapping, sample_rate, budget_usd, budget_period, enabled}`.

Rules are managed under `/online-rules` by project members (D-I-24). Saving a rule checks the filter with storage for the rule's view, the mapping against the evaluator (every variable of a judge prompt mapped; a check's `output`, and `expected` when it needs one; no `expected` or `item.*` expressions; no bare `span.*` in a trace rule) and the budget (judges only, with a priced model) (D-I-30). `target` and `evaluator_id` cannot change. Limits: a name of 1–128 characters, a filter of at most 4 096 bytes, 1–32 variables of at most 1 024 bytes each, `budget_usd` at most 1 000 000, and 500 rules per project (D-I-30). The checks against the evaluator run again in the rule's write transaction, and an evaluator PATCH runs them for every rule of the evaluator in its own write (D-I-19). On the Free plan an org has at most 1 active rule, an enabled one, paused or not; creating or enabling one past the limit is `403 plan_limit`, checked in the write transaction; a smaller limit never disables existing rules (D-I-32).

`POST /online-rules/preview` `{target, filter?, from?, to?}` counts what a filter matches in a window (the last 24 hours by default, at most 30 days), at most 10 000, with strict exclusions; the editor shows the count as the filter is typed (D-I-9).

### Triggering

Trace and span rules fire when the trace is complete: its root span has ended and 5 s passed without a new span, or `trace_completion_timeout_s` (project setting, 10–3600 s, default 60) passed without one (D-I-7). `commit` records traces in memory for projects with an enabled, unpaused rule; evaluation and experiment spans are never recorded; a flusher persists the changes every 5 s, at most 10 000 per tick, and shutdown persists all of them (D-I-8). Every 2 s a sweeper reads the enabled rules, takes each project's completed traces in batches of at most 500 (at most 2 000 per project per tick), and reads each rule's matches through storage (`trace_refresh` or `span_refresh`, at most 200 spans per trace) with strict exclusions, so a filter cannot reach evaluation or experiment traces (D-I-9, D-I-17). A read that fails is retried at the next tick; a trace is dropped after 5 failed ticks. A failed read spends no hourly-cap unit. Each chunk of at most 1 000 matches is admitted against the cap just before its write, so a failed write spends only its own chunk. A rule whose filter cannot be read is skipped with a warning, logged at most once per rule per hour. The sweeper stops between projects and between batches when the server shuts down. Traces that complete while a rule is disabled, paused, or not yet created are not evaluated by it; backfill is the catch-up.

**Sampling** is deterministic (D-I-10): SHA-256(`llmobs.sample.v1` ‖ rule id ‖ trace id ‖ span id for span rules), first 8 bytes big-endian, below `(rate · 2^64)`; a rate of 1 or more always samples. A span rule keeps the 50 earliest sampled spans of each trace, so re-processing never double-samples.

Live enqueues are capped at 10 000 per rule per UTC hour; extra matches are dropped and shown as `dropped_last_hour` (D-I-11).

### Execution

- **Jobs** (D-I-12): `queued → running → done | failed | queued | skipped`. One dispatcher claims jobs every second or when woken, by strict priority (manual, rule, backfill) and round-robin across projects, when a worker slot, the connection's concurrency slot and a token of its bucket are free (`max(1, rpm/6)` tokens, refilled at `rpm/60` per second); a webhook evaluator runs at most 4 jobs at once (D-I-6). A claim counts `claims += 1`; the worker counts `attempts += 1` right before the call. There is no lease: a janitor re-queues a `running` job older than 10 minutes, and startup re-queues every `running` job; both add 1 to `interruptions`, and a job interrupted a fourth time fails (`interrupted too often`, D-I-12). Every finish is fenced by `claims`, so a stale worker's result is dropped.
- **Steps** of a claimed job (§ I3.6): load the project and org (both active), the evaluator and, for a rule job, the rule (disabled or paused: re-queue); load the evaluator and, for a judge, its connection (changed since the claim: re-queue); parse the mapping; load the trace (no permit in 30 s: re-queue without a load retry; a missing trace or busy storage: a load retry, 30 s apart, 3 times); resolve the variables; check the live score definition; reject a levenshtein input over 64 KiB; for a judge or a webhook, check the org's monthly units cap (at the cap: skipped `quota`, no call, D-I-35); for a budgeted judge, render the prompt, estimate and reserve; count the attempt; run the evaluator; commit the judge trace; write the score with `Writer::Evaluator` (a score refused at the cap fails permanently, `monthly units cap reached`, and is not retried); finish, with `rule_spend`, the pauses and the breaker in one write, then a budget pause's queued jobs are skipped in chunks of 1 000.
- **Retries** (D-I-13): a retryable error backs off `min(5 s · 2^(attempts−1), 10 min) ± 20 %` and fails after 6 attempts. A permanent error fails at once. A failed result write is retried 3 times, 5 s apart, then takes the retryable path (`could not store the result`).
- **Pauses** (D-I-14): a connection error pauses every enabled rule of the connection (`connection`), marks the connection `failing` and re-queues the job with its attempt given back. A webhook error pauses the evaluator's rules (`errors`) and re-queues the job. A 404 (or `model_not_found`) for a judge whose model is not the connection's default pauses only the evaluator's rules (`errors`, detail = the error text) and leaves the connection `ok`; for the default model it is a connection error. A manual judge job fails and marks the connection (a manual 404 for a non-default model fails and leaves the connection alone); a manual webhook job fails. When the connection or key changed during the call, the job re-queues without a pause. A successful connection test or a key change resumes `connection` pauses.
- **Judge trace per attempt** (D-I-16): the judge trace id is SHA-256(`llmobs.judge-trace.v1` ‖ job id ‖ claims as u32 BE)[..16]; the score carries the job id and the evaluator version in its metadata (D-I-18).
- **Budget:** before a judge call, estimate cost (`ceil(prompt chars / 4)` input tokens + `max_output_tokens` output tokens at the model's base prices, `PriceTable::price_for`, tiers ignored). If `spent + reserved + estimate > budget_usd` for the current period, the rule is paused (`paused_reason='budget'`, UI banner, optional alert), its queued jobs are skipped, and the job is skipped. A model without a price pauses the rule (`errors`). The reservation settles to the actual cost after the call. The rule resumes automatically at the next period. A rule's spend is kept per UTC day (`rule_spend`); a month sums its days; reservations are in memory (D-I-15).
- **Error breaker** (D-I-14): a pause-class error (401, 402, 403 that is not a content filter, 404, quota exhausted) pauses the rule at once with `paused_reason='errors'`. Otherwise, when at least 20 of the rule's last 50 evaluated jobs exist and half or more of them failed, the rule pauses with `paused_reason='errors'`. Jobs that failed before the evaluator ran (a write failure, a score definition mismatch) do not count. A retried attempt, a re-queue and a pause that was downgraded to none do not count either: only the job's final outcome (`done` or `failed`) is recorded.
- **Rate limits per connection:** `max_concurrency` and `requests_per_minute` per connection, shared by every rule of every project (D-I-4, D-I-6).
- **Manual runs:** `POST /evaluators/{id}/run` queues one job at the highest priority, with the caller's mapping, and answers `202` with it; the UI polls `GET /eval-jobs/{id}`. Every variable of a judge's prompt must be mapped (`400` otherwise). Manual runs have no budget, count usage, and share the test runs' limit of 10 a minute per user (D-I-22).
- **Deleted targets:** a job whose target trace was deleted (D-M-6) ends `skipped` with error `deleted`; when the deletion lands during the judge call, the job keeps the call's cost and usage, and a judge trace or score it already committed is erased by a follow-up deletion of the same actor. Test runs and manual runs on a deleted trace answer `404`.

### Backfill

"Run on existing data" runs a rule over stored traces (D-I-25). `POST /online-rules/{id}/backfills/estimate` `{from, to, max_targets, sample_rate?}` answers how many targets the window holds (at most `max_targets`, and whether more matched), how many the sample keeps, and for a judge the tokens and cost: per target `ceil((template characters + 2 000 × mapped variables) / 4)` input tokens and `max_output_tokens` output tokens at the model's price (D-I-38, D-I-42). `POST /online-rules/{id}/backfills` with the same body starts one: the window is at most 30 days and ends at least 5 minutes ago, `max_targets` is 1–10 000, `sample_rate` defaults to the rule's; the rule must be enabled and unpaused, and a rule runs one backfill at a time (`409` otherwise).

A backfill runner, one of the eval loops, runs each backfill to its end, oldest first (D-I-39). It reads `trace_list` or `span_list` page by page, newest first, with the rule's filter and strict exclusions, so evaluation and experiment traces are never targets (D-I-17); the plan's data window applies (D-I-34). Each target is sampled as live targets are (D-I-10); targets the rule already scored are skipped by looking up their derived score ids; the rest are queued as backfill jobs at the lowest priority, re-queuing a failed or skipped job of the same target (D-I-11, D-I-40). The rule's budget applies when the jobs run (D-I-15), and queued backfill jobs expire after 7 days (D-I-28). A failed read fails the backfill and keeps the jobs already queued; a rule disabled during a run fails it (`rule disabled`); a rule whose filter, variable mapping, sample rate, target or evaluator changes during a run fails it (`rule changed`); a pause, resume or budget change does not, and an edit to the evaluator itself (its template or output) during a run is not detected; a deleted rule takes its backfills with it; a backfill running at shutdown is marked `failed` (`interrupted`) at the next start (D-I-41). There is no cancel; disabling the rule stops its jobs.

## Datasets

- Create (name, description, optional input/expected JSON Schemas).
- Items: `input` (any JSON), `expected_output` (optional JSON), `metadata`, optional `source_trace_id/source_span_id`.
- **Versions:** every mutation batch (add / edit / delete / import) creates version `current_version + 1` with a description. Reads default to the latest version; `?version=n` reads a past one.
- **Import:** CSV or JSONL upload (≤ 100 MiB), with a column mapping step: each column → `input.<key>` | `expected.<key>` | `metadata.<key>` | ignore; a single column can map to the whole `input`/`expected`. JSONL rows with `input`/`expected_output`/`metadata` keys import directly. Invalid rows are reported, valid ones imported (one version).
- **From traces:** "Add to dataset" on a trace or span copies `input` → item input and `output` → `expected_output` (editable before saving); records `source_*` (GDPR cascade, 03 §7).
- Export: JSONL / CSV of a version.

## Experiments

Client-side execution with the SDK (10); the server stores results and runs optional server evaluators.

### Protocol

1. `POST /experiments` `{name, dataset: name|id, dataset_version?, metadata, server_evaluators: [evaluator names]}` → `{id, dataset_version, items: [...]}` (items paginated if large).
2. SDK runs `task(item.input)` per item (and per repetition) under a root span named after the experiment, with `metadata['llmobs.experiment_id']`, `metadata['llmobs.dataset_item_id']`, `environment = "experiment"` (unless set). The trace is exported over OTLP as usual.
3. SDK runs local scorers → `POST /scores` with `experiment_id`, `dataset_item_id`, `trace_id`.
4. SDK `POST /experiments/{id}/items` (batches) `{dataset_item_id, repetition, trace_id, output, error, latency_ns}`.
5. SDK `POST /experiments/{id}/finish`. The server enqueues one `eval_job` per (item × server evaluator) with `not_before = now + 10 s` (lets the trace flush), variables resolved with `expected`/`item.*` available. Status becomes `completed` when all jobs are done or failed.

### Results & comparison

- Experiment page: table of items (input, expected, output, per-score values, latency, cost from the trace, link to trace); aggregates per score (mean, median, pass rate for booleans, distribution for categoricals); errors count.
- Compare 2–4 experiments on the same dataset: aggregate deltas, per-item side by side with regressions highlighted (score dropped by > threshold or boolean flipped to fail).
- API returns the same data for CI gating; the SDK exposes `result.summary()` and `result.assert_min("correctness", 0.8)` style helpers (10).

## Library

`llmobs-eval` implements evaluator execution, variable mapping, prompt templating and judge traces, and it has no database (D155). Given an evaluator, its variables or trace data, and a connection, it returns a score value and the normalized spans of the judge trace. The server track owns the job queue, the evaluation tables, secret encryption and the API handlers (D156).
