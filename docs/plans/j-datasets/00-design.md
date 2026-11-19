Status: implemented on 2026-11-16 (M1–M6, with a Sonnet review and fixes per milestone)

# J · Datasets and experiments — design

Track J adds what the MVP scope promises under "Datasets" and "experiment results/comparison" (`mvp-decisions.md` § MVP scope, D17, D18, D34):

- versioned datasets of items in `meta.db`, with CSV and JSONL import, JSONL and CSV export, and "Add to dataset" from a trace or a span;
- the experiments API: the SDK creates an experiment, runs the task on every item on the client (D18), sends the scores and the results, and finishes it;
- `cetra.evaluate(dataset, task, scorers)` in the Python and TypeScript SDKs, with a small REST client for datasets;
- the experiment page and the comparison of 2–4 experiments;
- the D93 SQL views `dataset_items` and `experiment_items` in the SQL console and MCP;
- the GDPR cascade of D24 and D-M-14 for items copied from traces;
- the plan data window (D175) on experiments.

**Read first:** `AGENTS.md`; `docs/mvp-decisions.md` (D17, D18, D24, D34, D93, D101, D173, D175, D178, D-F-17, D-I-17, D-I-34, D-M-6, D-M-14, D-P-44, D-P-47, Track J); specs 04 § Datasets & experiments, 05, 06 § Datasets, § Experiments, 09 § Datasets & experiments, 10 § Datasets and experiments, 11 § Datasets, § Experiments (all as amended by this plan); the competitor notes in § Competitors below.

This document holds the decisions, the data model, the API contract, the architecture and the milestones. Each milestone has its own plan of small sub-phases. **Haiku implements them one sub-phase at a time** with no context beyond the plan and the files it names; **Sonnet reviews** each milestone. If a plan and the code disagree (a name moved, a function changed), the implementer STOPS and reports instead of guessing.

## Milestones

| Plan | Milestone | Depends on | Runs in parallel with |
|---|---|---|---|
| [`01-m1-datasets.md`](01-m1-datasets.md) | **M1** The migration (every J table), the dataset store, `ProjectPerm::ManageDatasets`, dataset and item routes, versions, JSON Schema validation, OpenAPI, fixtures | — | nothing in J (it creates the shared modules) |
| [`02-m2-import-export.md`](02-m2-import-export.md) | **M2** CSV/JSONL import (preview and import), export, add-from-trace | M1 | M3 (§ Parallel work) |
| [`03-m3-experiments.md`](03-m3-experiments.md) | **M3** The experiment store and routes, experiment scores, the storage reads for results, results and compare, the data window, both SQL views, the whole deletion cascade | M1 | M2 |
| [`04-m4-sdks.md`](04-m4-sdks.md) | **M4** Python and TypeScript: `Client`, `datasets`, `evaluate`, `aevaluate` (Python), the experiment context | M3 merged (the API contract) | M5, M6 |
| [`05-m5-web-datasets.md`](05-m5-web-datasets.md) | **M5** Web: datasets list and page, item editing, versions, import wizard, export, "Add to dataset" on traces and spans | M2 merged | M4, M6 |
| [`06-m6-web-experiments.md`](06-m6-web-experiments.md) | **M6** Web: experiments list, experiment page, compare view; the end-to-end test; closeout (spec 11, roadmap, changelog) | M3 merged; the closeout sub-phase waits for M4 and M5 | M4, M5 (all but its closeout) |

Each sub-phase is one commit and ends with `just check` green.

## Decisions

The options marked "(user)" were chosen by the user on 2026-11-15. The others follow from those, from the specs or from the code. All are recorded in `docs/mvp-decisions.md` § Track J; where one changes a spec or an earlier decision, it says so and the spec is amended in the same change as this plan.

| # | Decision |
|---|---|
| D-J-1 | **A design doc and six milestones.** M1 datasets, M2 import/export/add-from-trace, M3 experiments (with both SQL views and the deletion cascade), M4 SDKs, M5 and M6 web. M1 creates every J table in one migration, so J takes one migration number. |
| D-J-2 | **Item shape (user).** An item has `input` (any JSON value, required), `expected_output` (any JSON value, optional) and `metadata` (a JSON object, default `{}`). A dataset may carry `input_schema` and `expected_schema` (JSON Schema, Draft 2020-12, through the workspace's `jsonschema` crate, each at most 64 KiB, compiled on save: an invalid schema is a 400). When set, every write is validated: add, patch, import rows and add-from-trace. `expected_schema` applies only when `expected_output` is present. Setting or changing a schema validates every live item of the latest version first, inside the write transaction; any failure is a 400 listing the first 20 (`items[<item_id>].input: <message>`). The check reads at most 64 MiB of item JSON: a larger dataset is a 400 `the dataset is too large to validate against a new schema` (the single meta writer must not stall). Writes that validate outside the write transaction (import, add-from-trace) re-read the schema texts in their write transaction and answer 409 `the dataset's schema changed; retry` when they differ. Schemas are dataset settings, not versioned. Free-form columns (Opik, LangWatch) were rejected: they need a column-type side structure and change the `evaluate` contract. |
| D-J-3 | **Versions (user).** Versions are integers. A new dataset is at version 0 with no items and no `dataset_versions` row. Every mutating request (add a batch, patch an item, delete an item, import, add-from-trace) makes version `current_version + 1` with one `dataset_versions` row: `description` (generated: `add 12 items`, `edit item <id>`, `delete item <id>`, `import <file name> (120 items)`, `add from trace <trace_id>`), `created_by` (the user id, or `key:<key id>` for an API key, or absent) and `created_at`. Item revisions use spec 04's validity ranges: a revision is in version *v* iff `valid_from_version ≤ v < coalesce(valid_to_version, ∞)`. A `PATCH` whose `input`, `expected_output` and `metadata` equal the live revision's (as `serde_json::Value`) creates no version and answers the live item. Reads take `?version=n` (0 ≤ n ≤ `current_version`, else 400); the default is `current_version`. Versions are kept forever; their cost is only the changed rows. No named tags. |
| D-J-4 | **Limits.** 1,000 datasets per project. A name is 1–200 characters after trimming, unique in the project (409), and must not parse as a UUID (400), so `{dataset}` in a path is the id when it parses as a UUID and the name otherwise. `description` at most 2,000 characters. At most 50,000 live items in the latest version (409 `a dataset holds at most 50000 items`). One item's `input` + `expected_output` + `metadata`, serialized, is at most 1 MiB, and each is nested at most 100 levels deep (400 `is nested too deeply`; deeper JSON could not be read back). A batch add holds 1–1,000 items, in a body of at most 16 MiB. |
| D-J-5 | **Permissions.** A new `ProjectPerm::ManageDatasets`: role `member`, key scope `write`, PATs allowed. It covers every dataset and experiment write (create, patch, delete, items, import, add-from-trace, experiment create, results, finish, delete). Reads (lists, items, versions, export, import preview, experiments, results, compare) use `ProjectPerm::View`, so a `read` key may read them, except the import preview: it is a `POST`, which D-H-4 refuses to `read` keys and read-only PATs (only `ProjectPerm::Query` routes read by `POST`). The SDK's key therefore needs `read` and `write` (and `ingest` for its spans and scores); spec 10 says so. Every write logs an audit event (`target: "audit"`) with the dataset or experiment id, the project id and the user or key id, never a name or content. |
| D-J-6 | **Import (spec 06, made precise).** Two calls. `POST …/datasets/{dataset}/import/preview?format=csv\|jsonl&partial=` takes the head of the file as an `application/octet-stream` body (the browser sends at most its first 1 MiB; `partial=true` drops the last, possibly cut, line) and answers `{format, columns, rows (≤ 20), errors, truncated}`; it writes nothing. `POST …/datasets/{dataset}/import?format=csv\|jsonl` takes `multipart/form-data` with a `file` part (≤ 100 MiB, ≤ 50,000 data rows) and a `mapping` JSON part, parses everything, validates each row, and writes the valid rows as one version in one transaction. CSV: RFC 4180 through the `csv` crate, the first record is the header (names non-empty and unique, at most 10,000 columns, else 400), each column maps to `input`, `input.<key>`, `expected`, `expected.<key>`, `metadata.<key>` or `ignore`; whole-field and keyed targets of one field cannot mix (400). A cell whose trimmed text starts with `{` or `[` and parses as JSON becomes that JSON, every other cell stays a string, and an empty cell leaves its key out. A row without any input key is a row error. JSONL: each non-empty line is an object with `input` (required), `expected_output` and `metadata` (an object); the export-only keys `id`, `source_trace_id` and `source_span_id` are ignored, so an export imports back; any other key is a row error (its name cut to 40 characters); the mapping part is absent. Row errors (`{row, message}`, 1-based data rows, at most 100 listed, `error_count` counts all) do not stop the import; no valid row is a 400 and no version, so a successful answer always has a version. The live-item cap counts the valid rows. The parse runs on `spawn_blocking`, and at most 2 imports run at once per process (a third is a 503); the permit is taken before the body is read, and the body must arrive within 60 s (else a 400 `the upload took too long`). With `partial=true` the preview always drops the last record it read (a cut can land inside a quoted multi-line cell), and a head with no data record left is a 400 `the head is too short to read`; the preview body has the same 60 s deadline. The file name in the version description has its control characters removed and is cut to 200 characters. Opik and LangWatch import asynchronously; at 100 MiB a synchronous parse and one SQLite transaction take a few seconds, which is accepted. |
| D-J-7 | **Export.** `GET …/datasets/{dataset}/export?version=&format=jsonl\|csv` streams the version's live items ordered by `item_id`, read in pages of 1,000. JSONL lines are `{id, input, expected_output?, metadata, source_trace_id?, source_span_id?}`. CSV columns are `id,input,expected_output,metadata,source_trace_id,source_span_id`, each JSON field as compact JSON text, lines ending in CRLF. A CSV round trip keeps objects and arrays; scalars come back as strings (only cells starting with `{` or `[` are parsed as JSON). `Content-Disposition: attachment; filename="<dataset id>-v<version>.<ext>"`. Cells are not escaped against spreadsheet formulas (a cell starting with `=`, `+`, `-` or `@`): the export is data for tools, and the page says to import it as text in a spreadsheet. |
| D-J-8 | **Add from trace (user: trace and span).** One "Add to dataset" button in the trace header, next to "Run evaluator", with a From choice: the trace root or the selected span (it mirrors the run-evaluator dialog instead of a second button in the span detail). `POST …/datasets/{dataset}/items/from-trace` `{trace_id, span_id?, input?, expected_output?, metadata?}`. The server loads the trace through `eval::trace_load::load_trace` with `Visibility::window` (the plan window applies, a hidden trace is a 404; the loader's caps and read permit apply; a truncated load is a 400 `the trace is too large to copy`) and refuses a trace the deletion guard blocks (404, checked inside the write transaction). Without `span_id` it takes the trace root by spec 06's `trace.input` rule; with it, that span (404 when absent). `input` and `expected_output` default to the span's decoded `input` and `output` (spec 06 § Variable mapping: `messages` and `json` as JSON, `text` as a string; an absent output leaves `expected_output` out; an absent input is a 400 `the span has no input`; an evaluation trace is a 400; busy storage is a 503), and the request's fields replace them (the dialog sends the edited values). It stores `source_trace_id` and `source_span_id` (the root's span id when none was given) and makes one version. The root is picked by a new `cetra-eval` function, `TraceData::root_span_id`, so the server and the judges share the rule. One item per call; no bulk action in the MVP. |
| D-J-9 | **Deletion cascade (D24, D-M-14).** In the meta write of `deletion/resolve.rs` that stores the scope (next to M8's `eval_jobs` delete), J deletes, for the deletion's project: every `dataset_items` revision whose `source_trace_id` is in the scope (all versions: edits copy the source ids forward, so every revision of such an item goes); every `experiment_items` row whose `trace_id` is in the scope; and every `experiment_items` row whose `revision_id` was one of the deleted revisions (its input snapshot came from the trace). The three deletes are set-based statements over the scope rows (`deletion_traces`) that the same transaction has just written, run only when the scope is stored, so an item added from a trace just before the write is never missed; add-from-trace checks the guard inside its own write transaction, so one added after it is refused. Project and org drops cascade through the foreign keys. Retention and the plan window never delete dataset or experiment rows: an experiment item whose trace has expired keeps its row, and its trace link answers 404. |
| D-J-10 | **Experiments (D18, user: snapshot).** `POST …/experiments` `{name, dataset, dataset_version?, description?, metadata?, repetitions?}` pins the dataset's version (default `current_version`; version 0 or no live item is a 400) and answers the experiment and the first page of the pinned version's items; the SDK pages the rest with `GET …/datasets/{dataset}/items?version=`. `repetitions` is 1–20 (default 1); live items × repetitions is at most 20,000 (400). `POST …/experiments/{id}/items` takes 1–500 results `{dataset_item_id, repetition, trace_id?, output?, error?, latency_ns?}`; the server copies the item's `input`, `expected_output` and `revision_id` from the pinned version (an item not in it, or a repetition ≥ `repetitions`, is a 400 for the batch). One row per (item, repetition): a second post replaces it and keeps the first `created_at`, so SDK retries are idempotent. `output` is any JSON of at most 256 KiB serialized, and the outputs of one request total at most 32 MiB (400; the body limit is 40 MiB; the SDK splits batches by count and by bytes); `error` at most 4,096 characters. A result whose `trace_id` the deletion guard blocks is not stored and counted in the answer's `skipped`; the guard is checked inside the write transaction. `POST …/experiments/{id}/finish` `{error?}` sets `completed` (or `failed` with the error) and `finished_at`; results after it are 409; a second finish with the same outcome answers the experiment unchanged (so an SDK retry is safe) and one with another outcome is 409. Create reads the dataset, counts the pinned version's items and inserts the experiment in one write transaction. An experiment that is never finished stays `running`; there is no janitor. `DELETE …/experiments/{id}` deletes the row and its results (not the traces or scores in storage). Names are 1–200 characters and not unique. `metadata` is an object of at most 64 KiB. Deleting a dataset sets its experiments' `dataset_id` to NULL; they keep their snapshots. |
| D-J-11 | **No server evaluators on experiments (user).** Experiments are scored by the SDK's scorers only. There is no `server_evaluators` field, no `eval_jobs` for experiments, and I's queue, executor and mapping checks are unchanged (`expected` and `item.*` stay refused, D-I-30). This amends spec 06 § Experiments step 5, spec 04's `experiments.server_evaluators` and spec 10's `server_evaluators=` argument, and D11's "LLM-as-judge (online + experiments)": judges score experiments only when the user calls one inside a scorer. |
| D-J-12 | **Experiment scores.** Scores stay rows of the columnar `scores` table (spec 04). `ScoreCreate` gains optional `experiment_id` and `dataset_item_id` (UUIDs): both or neither, a `trace` target only, and the experiment must exist in the route's project (one `meta.db` read per distinct experiment in the batch; any failure is a 400 for the batch, field `scores[i].experiment_id`). The stored `Score` carries both (`cetra-core` already has the fields; no core change). They count as units (D173) and go through the 402 cap like every score. An experiment score's stored `created_at` is never earlier than the experiment's (a client-minted UUIDv7 from a skewed clock would otherwise fall before the results read's lower bound). A derived score id (a client that omits `score_id`) has `experiment_id` as its author: the trace already identifies the item and the repetition (amends spec 06 § Deterministic score IDs). |
| D-J-13 | **Results and comparison (user: 2–4, first is the baseline).** A results or compare page stops at 16 MiB of input, expected and output text (always at least one row), so a page can hold fewer rows than `limit`; a compare cell holds the lowest repetition's output and the error of any repetition. Experiments of a deleted dataset compare when they share its name. An experiment with more than 500,000 scores or 100,000 traces cannot be summarized (400). The aggregation reads a light projection of the results (item, repetition, trace, error, latency); full input, expected and output text is read only for the page of rows answered. Two storage reads in `crates/storage/src/reads/experiments.rs` (a new file): `experiment_scores` (the live scores whose `experiment_id` is the experiment) and `experiment_traces` (the traces whose `metadata['cetra.experiment_id']` is the experiment: `trace_id`, `cost_total`, `duration_ns`, `error_count`, no default exclusions); both take the experiment's `created_at` minus 1 hour (client clocks stamp spans and score ids) as a lower time bound to prune files. The server aggregates in memory. Per score name: `numeric` mean, median, min, max, count; `boolean` pass rate (mean of 0/1) and count; `categorical` counts per label; `text` count only. Per experiment: item rows, rows with an error, latency mean and p50/p95 (from `latency_ns`), total cost (sum of trace costs). Per item, a score's value is the mean over its repetitions (booleans as 0/1, categoricals by `value_num` when set, else left out of the mean). `GET …/experiments/compare?ids=a,b[,c[,d]]&threshold=0.1`: 2–4 distinct ids of the same dataset (400 otherwise), the first is the baseline; per score the delta of each candidate's mean to the baseline's, computed over the items that both ran without error (each delta carries that `n_compared`; a `warnings` entry says when the item sets differ), so a candidate that fails on hard items does not look better; per item one cell per experiment that has a result for it (cells are matched by `experiment_id`, never by position); an item regresses in a candidate when one of its score means is lower than the baseline's by more than `threshold` (absolute, 0–1000, default 0.1) or when the baseline has no error and the candidate has one. Higher is better for every score; a score where lower is better shows its regressions inverted (contract gap CG-J1). Different dataset versions answer a `warnings` entry, not an error. |
| D-J-14 | **The plan window (user).** Datasets are curated assets like saved views: never hidden, no gate, no count limit by plan. An experiment whose `created_at` is before the org's `visible_from` (`Visibility::window(state, org).visible_from()`, through one `pub(crate) fn visible_from(state, org)` in `crates/server/src/experiments/mod.rs`) is hidden: 404 on its routes except `DELETE` (deleting reveals nothing, and the rows hold outputs), absent from lists, refused by compare (404), and absent from the `experiment_items` SQL view. Its traces and scores are hidden by storage already (D-P-47). No feature gate on any plan (D178 lists none for J). |
| D-J-15 | **SQL views (D93).** Built per query, only when the statement names the view (a case-insensitive substring test of the SQL text, before parsing), and registered with `QueryContext::register_sqlite_view` in `POST …/sql`, its exports and the MCP `query_sql` tool. `dataset_items`: the live items of every dataset of the project at its `current_version`, columns `dataset_id`, `dataset_name`, `dataset_version` (Int64), `item_id`, `input`, `expected_output`, `metadata` (Utf8 JSON text), `source_trace_id`, `source_span_id` (Utf8 hex), `created_at` (Timestamp ns UTC). `experiment_items`: rows of the project's visible experiments, columns `experiment_id`, `experiment_name`, `dataset_id`, `dataset_version`, `dataset_item_id`, `repetition` (Int64), `trace_id`, `input`, `expected_output`, `output`, `error`, `latency_ns` (Int64), `created_at`. D101's window applies to `experiment_items.created_at`, not to `dataset_items`. More than 1,000,000 rows or more than 256 MiB of text is a 400 `query_failed` (`dataset_items is too large for the SQL console`); text columns are `LargeUtf8`, so no offset overflow can panic. The server owns both catalogs and adds them to `GET /schema`. |
| D-J-16 | **SDK (D-F-17: hand-written).** Python `cetra.Client` (`requests`, the same config resolution, `Retry-After`, the 402 pause) with `client.datasets.{create, get, list, delete}` and `Dataset.add_items`; TypeScript `new cetra.Client()` with the same over `FetchLike`. `evaluate` needs `cetra.init()` (else `ConfigError`). Per item and repetition it runs the task inside an experiment context that stamps `cetra.experiment.id`, `cetra.dataset_item.id` and the environment `experiment` on every span started in it (the `cetra.environment` stamp of `init` is overridden for those spans only), under a root span named after the experiment, of kind `chain`, with the item input. Scorers run in the SDK; their scores go through the existing score queue with `experiment_id` and `dataset_item_id`. Results are posted in batches of up to 500; `finish` follows a `flush()`. The REST client retries (network error, timeout, 429, 5xx, `Retry-After`) only GET, DELETE, the results post (an upsert) and finish (idempotent, D-J-10); creates and item writes are sent once. It raises `ApiError` on any error answer, 402 included (REST calls are not units, D-J-18; only the score queue keeps its 402 pause); task and scorer exceptions are recorded per run and never raise. A scorer's name is its function's name; lambdas and duplicate names are refused before any request. Each run starts from an empty context, so its root is a new trace without the caller's session, user, tags or metadata. `summary()` prints the experiment id, not a UI link (an `sk` key cannot learn its org slug). With `sample_rate` below 1, unsampled runs have no trace and no scores; `evaluate` warns once. Python tasks are `task(input)` or `task(input, item)`; TypeScript tasks are always `task(input, item)`. Details in M4. |
| D-J-17 | **UI (user: compare 2–4).** Routes use ids: `/:org/:project/datasets`, `/datasets/:datasetId`, `/experiments`, `/experiments/:experimentId`, `/experiments/compare?ids=` (amends spec 11's `/datasets/:name`). JSON is edited in a monospace textarea checked with `JSON.parse` (no new editor dependency). The text diff of outputs against the baseline is a hand-written word-level LCS (no diff dependency), skipped above 2,000 words a side. Details in M5 and M6. |
| D-J-18 | **Units and entitlements.** Dataset and experiment rows are not units (D173 counts spans and scores). The experiment's spans and its scores are units through ingest and `scores::write`. |
| D-J-19 | **Migration number.** J's migration takes the next free number in `crates/server/migrations/meta/` when M1 is implemented (`0016_datasets.sql` if `0015_email_verified_via.sql` is still the last; T's plan reserved 0015 before `0015_email_verified_via` landed, so T and J both pick at implementation time). First to merge keeps the number; the other renumbers its file at merge time. |

## Data model

One migration, `NNNN_datasets.sql` (D-J-19), replaces spec 04 § Datasets & experiments:

```sql
CREATE TABLE datasets (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  description TEXT,
  input_schema TEXT,                  -- JSON Schema text, or NULL
  expected_schema TEXT,
  current_version INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);

CREATE TABLE dataset_versions (
  dataset_id TEXT NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  description TEXT NOT NULL,
  created_by TEXT,                    -- user id, or 'key:<key id>'
  created_at INTEGER NOT NULL,
  PRIMARY KEY (dataset_id, version)
);

CREATE TABLE dataset_items (
  revision_id TEXT PRIMARY KEY,
  item_id TEXT NOT NULL,              -- stable across revisions
  dataset_id TEXT NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
  input TEXT NOT NULL,                -- JSON
  expected_output TEXT,               -- JSON, NULL when absent
  metadata TEXT NOT NULL DEFAULT '{}',
  source_trace_id BLOB,               -- 16 bytes
  source_span_id BLOB,                -- 8 bytes
  valid_from_version INTEGER NOT NULL,
  valid_to_version INTEGER,
  created_at INTEGER NOT NULL
);
CREATE INDEX dataset_items_live ON dataset_items(dataset_id, valid_to_version, item_id);
CREATE INDEX dataset_items_item ON dataset_items(dataset_id, item_id, valid_from_version);
CREATE INDEX dataset_items_source ON dataset_items(source_trace_id) WHERE source_trace_id IS NOT NULL;
CREATE UNIQUE INDEX dataset_items_one_live ON dataset_items(dataset_id, item_id) WHERE valid_to_version IS NULL;

CREATE TABLE experiments (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  dataset_id TEXT REFERENCES datasets(id) ON DELETE SET NULL,
  dataset_name TEXT NOT NULL,         -- copied at create, shown after the dataset is deleted
  dataset_version INTEGER NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  metadata TEXT NOT NULL DEFAULT '{}',
  repetitions INTEGER NOT NULL DEFAULT 1,
  item_count INTEGER NOT NULL,        -- live items of the pinned version
  status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
  error TEXT,
  created_by TEXT,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX experiments_project ON experiments(project_id, created_at);
CREATE INDEX experiments_dataset ON experiments(dataset_id, created_at);

CREATE TABLE experiment_items (
  experiment_id TEXT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
  dataset_item_id TEXT NOT NULL,      -- item_id (stable)
  repetition INTEGER NOT NULL,
  revision_id TEXT NOT NULL,          -- the pinned revision; no FK (D-J-9 deletes by it)
  input TEXT NOT NULL,                -- snapshot of the revision
  expected_output TEXT,
  trace_id BLOB,
  output TEXT,                        -- JSON
  error TEXT,
  latency_ns INTEGER,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (experiment_id, dataset_item_id, repetition)
);
CREATE INDEX experiment_items_trace ON experiment_items(trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX experiment_items_revision ON experiment_items(revision_id);
```

Changes from spec 04: `dataset_versions.description` is required; `dataset_items` gets two more indexes; `experiments` loses `server_evaluators` (D-J-11) and gains `dataset_name`, `repetitions`, `item_count`, `error`, `created_by`, and `dataset_version` becomes required; `experiment_items` gains the snapshot (`revision_id`, `input`, `expected_output`) and loses `cost_total` (read from the trace, D-J-13). Times are nanoseconds; ids are UUIDv7 text; trace and span ids are raw bytes.

**Writing a version** (one helper in `meta/datasets.rs`, used by every mutation): inside the write transaction, read `current_version` (the row is locked by `BEGIN IMMEDIATE`), set `v = current_version + 1`, close the replaced or deleted revisions with `valid_to_version = v`, insert the new revisions with `valid_from_version = v`, insert the `dataset_versions` row, and set `datasets.current_version = v, updated_at = now`.

**Reading version *v*:** `WHERE dataset_id = ? AND valid_from_version <= ? AND (valid_to_version IS NULL OR valid_to_version > ?)`, ordered by `item_id`, cursor `item_id > ?`.

## API contract

All routes are under `/api/v1/projects/{project}` and follow spec 09's conventions (problem+json, `{data, next_cursor}` lists, `limit` 1–1000 default 50, RFC 3339 times with 9 digits, trace ids in lowercase hex). `{dataset}` is an id or a name (D-J-4).

| Method & path | Perm | Body → answer | Milestone |
|---|---|---|---|
| `GET /datasets` | View | `?limit&cursor&q` → `Page<Dataset>` ordered by lowercased name then id (`q`: case-insensitive name substring) | M1 |
| `POST /datasets` | ManageDatasets | `CreateDatasetRequest` → `201 Dataset` | M1 |
| `GET /datasets/{dataset}` | View | → `Dataset` | M1 |
| `PATCH /datasets/{dataset}` | ManageDatasets | `UpdateDatasetRequest` → `Dataset` | M1 |
| `DELETE /datasets/{dataset}` | ManageDatasets | → `204` | M1 |
| `GET /datasets/{dataset}/versions` | View | `?limit&cursor` → `Page<DatasetVersion>` newest first | M1 |
| `GET /datasets/{dataset}/items` | View | `?version&limit&cursor` → `Page<DatasetItem>` by `item_id` | M1 |
| `POST /datasets/{dataset}/items` | ManageDatasets | `AddItemsRequest` → `201 AddItemsResponse` | M1 |
| `GET /datasets/{dataset}/items/{item_id}` | View | `?version` → `DatasetItem` | M1 |
| `PATCH /datasets/{dataset}/items/{item_id}` | ManageDatasets | `UpdateItemRequest` → `DatasetItem` | M1 |
| `DELETE /datasets/{dataset}/items/{item_id}` | ManageDatasets | → `200 DatasetVersion` | M1 |
| `POST /datasets/{dataset}/import/preview` | View | raw body ≤ 1 MiB, `?format` → `ImportPreview` | M2 |
| `POST /datasets/{dataset}/import` | ManageDatasets | multipart → `201 ImportResult` | M2 |
| `GET /datasets/{dataset}/export` | View | `?version&format` → stream | M2 |
| `POST /datasets/{dataset}/items/from-trace` | ManageDatasets | `FromTraceRequest` → `201 DatasetItem` | M2 |
| `GET /experiments` | View | `?dataset&limit&cursor` → `Page<Experiment>` newest first | M3 |
| `POST /experiments` | ManageDatasets | `CreateExperimentRequest` → `201 CreateExperimentResponse` | M3 |
| `GET /experiments/{id}` | View | → `Experiment` | M3 |
| `DELETE /experiments/{id}` | ManageDatasets | → `204` | M3 |
| `POST /experiments/{id}/items` | ManageDatasets | `ExperimentResultsRequest` → `200 ExperimentResultsResponse` | M3 |
| `POST /experiments/{id}/finish` | ManageDatasets | `FinishExperimentRequest` → `Experiment` | M3 |
| `GET /experiments/{id}/results` | View | `?errors_only&limit&cursor` → `ExperimentResults` | M3 |
| `GET /experiments/compare` | View | `?ids&threshold&regressions_only&limit&cursor` → `ExperimentComparison` | M3 |

Types (Rust in `crates/server/src/api/datasets.rs` and `api/experiments.rs`, `snake_case` JSON, optional fields omitted when absent):

```text
Dataset            { id, name, description?, input_schema?, expected_schema?, current_version: i64,
                     item_count: i64 (live items at current_version), experiment_count: i64,
                     created_at, updated_at }
CreateDatasetRequest { name, description?, input_schema?, expected_schema? }      (deny_unknown_fields)
UpdateDatasetRequest { name?, description?, input_schema?, expected_schema? }      (a field set to
                     JSON null clears it; absent leaves it: Option<Option<…>> with serde_with-free
                     `#[serde(default, deserialize_with = …)]` as in api/online_rules.rs, or the
                     house pattern the M1 plan names)
DatasetVersion     { version: i64, description, created_by?, created_at }
DatasetItem        { id (item_id), revision_id, version: i64 (valid_from_version), input,
                     expected_output?, metadata, source_trace_id?, source_span_id?, created_at }
NewDatasetItem     { input, expected_output?, metadata? }
AddItemsRequest    { items: [NewDatasetItem] (1–1000) }
AddItemsResponse   { version: i64, item_ids: [Uuid] }          (request order)
UpdateItemRequest  { input?, expected_output?, metadata? }     (absent keeps; expected_output: null clears)
ImportPreview      { format: "csv"|"jsonl", columns: [String], rows: [Value] (a CSV row is an array of
                     strings, a JSONL row the parsed object), errors: [{ row, message }], truncated: bool }
ImportMapping      { columns: [{ column, target }] }           (target: "input" | "input.<key>" |
                     "expected" | "expected.<key>" | "metadata.<key>" | "ignore")
ImportResult       { version: i64, imported: i64, error_count: i64, errors: [{ row, message }] }
FromTraceRequest   { trace_id, span_id?, input?, expected_output?, metadata? }
Experiment         { id, name, description?, dataset_id?, dataset_name, dataset_version: i64,
                     metadata, repetitions: i64, item_count: i64, result_count: i64,
                     status: "running"|"completed"|"failed", error?, created_at, finished_at? }
CreateExperimentRequest  { name, dataset (id or name), dataset_version?, description?, metadata?,
                     repetitions? }
CreateExperimentResponse { experiment: Experiment, items: [DatasetItem], next_cursor }   (first 1000
                     items, flat: a nested generic page has no OpenAPI name)
ExperimentResult   { dataset_item_id, repetition: i64, trace_id?, output?, error?, latency_ns? }
ExperimentResultsRequest  { results: [ExperimentResult] (1–500) }
ExperimentResultsResponse { stored: i64, skipped: i64 }
FinishExperimentRequest   { error? }
ScoreSummary       { name, data_type, count: i64, mean?, median?, min?, max?, pass_rate?,
                     labels: { label: count } }
ExperimentSummary  { experiment: Experiment, scores: [ScoreSummary], result_count, error_count,
                     latency_mean_ns?, latency_p50_ns?, latency_p95_ns?, cost_total? }
ResultRow          { dataset_item_id, repetition, input, expected_output?, output?, error?,
                     trace_id?, latency_ns?, cost_total?, scores: { name: ResultScore } }
                     (input, expected_output and output are any JSON: the OpenAPI schema is
                     utoipa's AnyValue, never Object; M1 names the one house form)
ResultScore        { value_num?, value_str?, data_type }   (core already has a `ScoreValue`)
ExperimentResults  { summary: ExperimentSummary, rows: [ResultRow], next_cursor }
ExperimentComparison { experiments: [ExperimentSummary] (baseline first),
                     deltas: [{ experiment_id, scores: { name: { delta: f64, n_compared: i64 } } }] (candidates only),
                     warnings: [String], rows: [CompareRow], next_cursor }
CompareRow         { dataset_item_id, input, expected_output?,
                     cells: [{ experiment_id, output?, error?, trace_id?, latency_ns?,
                               scores: { name: f64 }, regressed: bool }] }   (one per experiment
                     with a result for the item; match by experiment_id)
```

M1–M3 settle the exact Rust (field docs, `ToSchema` names, which `Option`s, validation messages) and must keep these shapes. A deviation is reported, and the design doc is updated in the same commit.

`ScoreCreate` (`crates/server/src/api/scores.rs`) gains `experiment_id?: Uuid` and `dataset_item_id?: Uuid` (M3, D-J-12).

## Architecture

```
crates/server/migrations/meta/NNNN_datasets.sql      every J table                          (M1)
crates/server/src/meta/datasets.rs                   dataset, version and item store        (M1; M2 adds import/export reads)
crates/server/src/meta/experiments.rs                experiment and result store            (M3)
crates/server/src/api/datasets.rs                    API types of datasets                  (M1, M2)
crates/server/src/api/experiments.rs                 API types of experiments               (M3)
crates/server/src/datasets/mod.rs                    validation: names, sizes, JSON Schema  (M1)
crates/server/src/datasets/import.rs                 CSV/JSONL parsing and mapping          (M2)
crates/server/src/datasets/sql_views.rs              the two MemTables and their catalog    (M3)
crates/server/src/experiments/mod.rs                 results and compare aggregation        (M3)
crates/server/src/routes/datasets.rs                 dataset routes                         (M1, M2)
crates/server/src/routes/experiments.rs              experiment routes                      (M3)
crates/server/src/deletion/resolve.rs                + the J cascade                        (M3)
crates/server/src/scores.rs, api/scores.rs           + experiment fields                    (M3)
crates/server/src/routes/sql.rs, mcp/sql.rs          + view registration                    (M3)
crates/storage/src/reads/experiments.rs              experiment_scores, experiment_traces   (M3)
sdks/python/src/cetra/_client.py, _datasets.py, _experiments.py                            (M4)
sdks/typescript/src/client.ts, datasets.ts, experiments.ts                                  (M4)
web/src/routes/$org/$project/datasets/…, experiments/…, components/datasets/…, components/experiments/…   (M5, M6)
```

`crates/server/src/datasets/` and `experiments/` are plain modules of functions, no traits. M2 adds `TraceData::root_span_id` to `crates/eval/src/mapping/resolve.rs` (D-J-8). `jsonschema` (M1) and `datafusion` (M3, for the MemTables) move from `cetra-server`'s dev-dependencies to its dependencies, versions unchanged. M3 edits storage's `catalog_matches_the_schema_fixture` test to skip the two server-owned views in `fixtures/api/static/schema.json`.

## Parallel work

J runs while S (cloud auth), T (model prices), N's M3 (packaging) and R (storage hardening) are implemented. Shared files and the rule for each:

| File | Touched by J | Also touched by | Rule |
|---|---|---|---|
| `crates/server/migrations/meta/` | M1 | T | D-J-19: the next free number at implementation time; the second to merge renumbers. |
| `crates/server/tests/snapshots/meta_schema.snap` | M1 | T | Regenerate after a rebase. |
| `crates/server/src/api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | M1, M2, M3 | S, T, N M3 | Additive; after a rebase run `just openapi` and commit both generated files. Never hand-merge them. |
| `crates/server/src/http/access.rs` (route list), `routes/mod.rs`, `api/mod.rs`, `auth/roles.rs` | M1–M3 | S, T | Additive lines; keep alphabetical order. |
| `fixtures/api/manifest.json`, `fixtures/api/static/schema.json` | M1–M3 | T | Regenerate with `just api-examples` after a rebase. |
| `crates/server/src/api_examples.rs`, `http/access_suite.rs`, `crates/server/tests/openapi.rs` (its `OPERATIONS`, `TAGS`, `QUERY_OR_BODY` and page lists) | M1–M3 | S, T | Additive. |
| `crates/server/src/deletion/resolve.rs` | M3 | — | Only M3 in J touches it. |
| `crates/server/src/routes/sql.rs`, `mcp/sql.rs` | M3 | — | Only M3. |
| `crates/storage/src/reads/mod.rs`, `crates/storage/src/lib.rs` (`pub use reads::{…}`), `crates/storage/src/views/catalog.rs` (one test) | M3 | R | Additive; the new reads live in their own file. |
| `Cargo.toml` (root) | M2 (`csv`, axum's `multipart` feature) | N M3 | Additive; no existing pin changes. |
| `web/src/components/shell/nav.ts`, `web/src/mocks/writes.ts`, `web/src/test/fixtures.ts` | M5, M6 | T | Additive lines. |
| `web/src/components/trace/trace-header.tsx`, `span-detail.tsx` | M5 | — | — |
| `sdks/` | M4 | — | Only J. |
| `CHANGELOG.md`, `docs/roadmap.md` | each milestone's last sub-phase | everyone | Additive lines. |

M2 and M3 can run at the same time in two worktrees: they share only the additive files above. M4, M5 and M6 can run at the same time: M4 is only `sdks/` (and `tools/e2e` is M6's), and M5 and M6 share `nav.ts`, `writes.ts`, `fixtures.ts` and `CHANGELOG.md`. M6's J6.2 adds an Experiments tab to M5's dataset page: if M5 is not merged yet, that tab moves to J6.5.

## Fixed identifiers (fixtures and tests)

`fixtures/api` gets an example dataset and experiment for the example project (`…0001`, org `…0010`). The README lists the ranges in use (`…05xx` evaluators, `…06xx` score definitions, `…07xx` rules, `…08xx` jobs, `…0bxx` backfills). J takes:

| Range | Use |
|---|---|
| `01900000-0000-7000-8000-000000000cxx` | datasets (`…0c01` = `faq`) |
| `01900000-0000-7000-8000-000000000dxx` | dataset items (`…0d01`–`…0d03`) and revisions (`…0d11`–`…0d14`) |
| `01900000-0000-7000-8000-000000000exx` | experiments (`…0e01` baseline, `…0e02` candidate) |

M1 checks these ranges are free (`grep -rn "00000000c0\|0000000000c\|0000000000d\|0000000000e" fixtures/api` before using them) and stops if not. The exact uuids follow the README's format; the M1 plan writes them out.

## Competitors

From their source in `/home/sebastien/github` (2026-11-15):

- **Item shape.** Langfuse and Phoenix keep input / expected / metadata; Langfuse added optional JSON Schemas in late 2025. Opik and LangWatch use free-form columns with a column-type side structure; LangWatch migrated from a fixed shape to it.
- **Versions.** Langfuse uses validity ranges with a stable id (its API takes a timestamp as the version); Phoenix a revision log with integer versions; Opik and LangWatch whole-dataset snapshots, which forced LangWatch into retention and thinning rules. Every one that added versioning late needed a primary-key change or a backfill migration.
- **Experiments pin the version and snapshot the item** in Phoenix (`experiments_dataset_examples`), Laminar and LangWatch; Langfuse had to drop its run-item → item foreign key when versions came.
- **Source links** are plain columns everywhere (Phoenix: `SET NULL`); none erases the copied content when the source trace is deleted. J does (D-J-9).
- **Server-side scoring of experiments** exists in Phoenix (`EVAL_ONLY` jobs), Opik (manual evaluation of an experiment's traces) and Langfuse (eval rules on experiment observations). J leaves it out of the MVP (D-J-11).
- **Comparison.** Phoenix and Langfuse have an explicit baseline plus N runs, deltas and per-item side by side; Langfuse warns when runs differ. Laminar compares two.

## Contract gaps

- **CG-J1.** Scores have no direction: a regression is a lower mean. A "lower is better" flag on score definitions is a later change.
- **CG-J2.** An experiment of more than 20,000 rows (items × repetitions) is refused; the SDK can run a subset by passing a smaller dataset. Results are aggregated in memory from one storage read per page view.
- **CG-J3.** No server-side experiment runs and no server evaluators on experiments (D18, D-J-11).
- **CG-J4.** No bulk "add to dataset" from the SQL console; no named version tags. (Amended by D-J-25: bulk add from the trace list is in the MVP.)
- **CG-J5.** `running` experiments abandoned by a crashed SDK stay `running`; the UI shows their age.

## Conventions

- **Time**: `i64` nanoseconds in Rust and `meta.db`; RFC 3339 with 9 fractional digits in JSON (`cetra_core::time_serde`).
- **IDs**: UUIDv7 (`Uuid::now_v7()`), stored as `TEXT`; trace and span ids as `BLOB` in `meta.db`, lowercase hex in JSON.
- **Errors**: problem+json through `crate::http::error::ApiError`; field errors name the JSON path (`items[3].input`, `results[0].repetition`).
- **Privacy**: types that hold item content (`ItemRow`, `ItemValues`, `NewRevision`, `NewDatasetItem`, the import's parsed rows) get a hand-written `Debug` that prints ids and sizes only. Logs carry dataset, item, experiment, project, user and key ids, counts and sizes; never a name, a description, a schema or any item content (AGENTS.md § Privacy).
- **No `unwrap`/`expect`** outside tests unless a comment states the invariant.
- **Tests**: Rust tests in the module they test; HTTP tests through `TestApp`. Web tests use MSW with `server.use`, never `vi.mock`. SDK tests use the existing HTTP recorder (Python) and the injected `FetchLike` (TypeScript).

## Commands

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
just check
```

When a sub-phase changes the API contract, run `just openapi` and commit `crates/server/openapi.json` and `web/src/api/schema.gen.ts`; when it changes `fixtures/api/`, run `just api-examples` and commit the generated files.
