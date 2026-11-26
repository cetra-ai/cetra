# 05 — Query: filter DSL, SQL console, search, dashboard queries

Structured reads (`cetra-storage::reads`) compile directly to `cetra-query` push pipelines over `ScanSource` (`ParquetSource`, `MemtableSource`, `TracesSource`, `ScoresSource`), without SQL planning. The Filter DSL (`cetra-query::filter`) and Shape B SQL console/MCP (`cetra-query::sql`) parse with `sqlparser` (`GenericDialect`), rewrite hex-literal ID comparisons to binary ones, apply default exclusions, and execute on `WorkerPool` using `GlobalPool` / `ScopedPool` (`FairSpill`), `SpillManager`, and `CancelToken` (D-QE-1, D-QE-4, D-QE-5). `QueryContext::new` (in `cetra-storage`) wraps the `ProjectQuery` returned by `Storage::query` with an optional SQL `TimeWindow` and registers server-owned SQLite views as `BatchSource` tables (D91).

## Logical views exposed to users

| View | Rows | Backed by |
|---|---|---|
| `spans` | one per span | `ParquetSource` + `MemtableSource` |
| `traces` | one per trace (merged; the merge bookkeeping columns `last_ingest_time` and `partial_count` are not exposed) | `TracesSource` |
| `sessions` | one per session_id | `SELECT session_id, min(start_time) AS start_time, max(end_time) AS end_time, count(*) AS trace_count, sum(error_count) AS error_count, sum(total_tokens) AS total_tokens, sum(cost_total) AS cost_total, first non-null user_id by (start_time, trace_id) AS user_id, sorted distinct union of tags AS tags, duration_ns, duration_ms … FROM traces WHERE session_id IS NOT NULL GROUP BY session_id` (using dedicated internal `sessions` aggregates in `cetra-query`) |
| `scores` | one per live score | `ScoresSource` |
| `experiment_items` | one per experiment result (item × repetition) of the project's visible experiments, with the input snapshot | SQLite rows exposed via an in-memory `BatchSource` built per query, only when the statement names the view, at most 1M rows and 256 MiB of text (else `query_failed`). The server owns the schema, the rows and the column catalog, and registers the table through `QueryContext::register_sqlite_view` (D93). D101's window applies to `created_at`; experiments older than the data window are left out (D-J-14, D-J-15) |
| `dataset_items` | the live items of every dataset of the project at its latest version | same `BatchSource` approach; no time window (D-J-15) |

Cost totals of `sessions`, of the user list, and of the dashboard and alert queries (§ Dashboard queries; spec 07) are summed as `Decimal128(38, 18)`, so they do not depend on the partitioning (D-A2-4).

Each view exposes the stored columns of its table in spec 01 order, without `project_id`. `traces` also drops `last_ingest_time` and `partial_count`. Binary IDs (`trace_id`, `span_id`, `parent_span_id`, `root_span_id`) are lowercase hex `Utf8`. `duration_ms` follows `duration_ns`: on `spans`, `duration_ns` is stored; on `traces` and `sessions`, `duration_ns = end_time − start_time` in nanoseconds, placed after `end_time`, and `duration_ms = duration_ns / 1e6`. `events` and `links` keep their typed `cetra-column` `Events` and `Links` column types (D-QE-11) and are listed as `json` in `GET /schema` and `POST /sql` because clients receive them as JSON (D-QE-15).

`sessions` columns, in order: `session_id, start_time, end_time, duration_ns, duration_ms, trace_count, error_count, total_tokens, cost_total, user_id, tags`. `user_id` is the first non-null `user_id` of the session's traces ordered by `(start_time, trace_id)`. `tags` is the sorted distinct union of the traces' tags.

An optimizer rule rewrites `id = '<hex>'`, `id <> '<hex>'`, `id IN ('<hex>', …)` and `id NOT IN (…)` on these ID columns into comparisons on the stored binary bytes (`FixedBytes16` / `FixedBytes8`), so `ScanSpec` and `ParquetSource` can prune with bloom filters and page indexes. The literal must be exactly twice the ID's byte width and contain only hex digits, in either case. Otherwise the comparison stays on the hex string, which never matches an invalid ID. A comparison between two ID columns of the same plan node compares bytes. Joins between two views on an ID compare the hex strings: correct, but not pruned.

Column reference for each view is generated from the `cetra-column` view schemas and served at `GET /api/v1/projects/{p}/schema` (used by the UI autocomplete, the docs and the MCP `get_schema` tool).

## Default exclusions

Unless the request opts in (`include_eval=true` / `include_experiments=true`, or the filter explicitly constrains `environment`), list pages, user aggregates, dashboard series and alert queries add `environment IS DISTINCT FROM` each reserved environment (`'cetra-eval'`, `'cetra-playground'` and `'cetra-assistant'`, D-V-33, D-AS-11) and exclude experiment traces (`metadata['cetra.experiment_id'] IS NULL`). "The filter explicitly constrains `environment`" means the filter text references the `environment` column anywhere. Then the reserved-environment exclusions are not added; the experiment exclusion still is.

The exclusions apply to the view the request reads: `traces` and `spans` directly; session and user lists through the traces they aggregate; span-based and score-based metrics through a semi-join on the excluded traces.

**Strict exclusions** (D-I-9). Online-rule matching (`trace_refresh`, `span_refresh`, and the trace and span lists used by preview and backfill) sets `strict_exclusions`: evaluation, playground, assistant and experiment traces are left out whatever the filter names. On spans the test is on the span's trace: `trace_id NOT IN (SELECT trace_id FROM traces WHERE environment IN ('cetra-eval', 'cetra-playground', 'cetra-assistant') OR metadata['cetra.experiment_id'] IS NOT NULL)`.

**Match counts** (D-I-9). The online-rule preview counts the traces or spans that a filter matches in a window with strict exclusions, stopping at 10 001 rows (`match_count`), so the editor can show "10 000 or more" without reading every row. A spans count with strict exclusions also reads the project's traces to leave out evaluation and experiment traces, and that subquery is not bounded by the window. On a large project such a count can therefore hit its 10 s timeout and answer `422 query_timeout`. This is accepted: the editor then says the count could not be loaded. With strict exclusions, `trace_refresh` returns only `trace_id` and `start_time`, the two columns the trigger needs.

The SQL console applies **no** implicit exclusion (users see raw data; the schema docs explain the reserved environments).

## Filter DSL

A **SQL boolean expression** — the body of a `WHERE` clause — evaluated against one view (`traces`, `spans`, `sessions` or `scores` depending on the screen).

```
status = 'error' AND model LIKE 'gpt-5%'
cost_total > 0.10 AND tags @> ['beta']
metadata['customer'] = 'acme' AND duration_ns > 5e9
start_time > now() - INTERVAL '1 hour'
score('hallucination') >= 0.5
input ILIKE '%refund%'
```

### Grammar

Parsed with `sqlparser::Parser::parse_expr` (`GenericDialect`), then validated:

- **Identifiers:** bare column names of the target view, case-insensitive unless double-quoted. No qualified names.
- **Map access:** `metadata['k']`, `attributes['k']`, `resource_attributes['k']`, with a string literal key, on the views that have the column.
- **Operators:** `= != <> < <= > >= AND OR NOT`, unary `-`, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, `IN (…)`, `BETWEEN`, `LIKE`, `ILIKE`, arithmetic. `@>` (array contains) compiles to `array_has_all`.
- **Casts:** `CAST(x AS t)` and `x::t` with `t` ∈ `BIGINT, DOUBLE, VARCHAR, BOOLEAN, TIMESTAMP`, compiled to `TRY_CAST` (D100).
- **Literals:** strings, numbers (incl. `5e9`), booleans, `NULL`, `INTERVAL '…'`, array literals.
- **Functions (allow-list):** `now()`, `lower`, `upper`, `length`, `coalesce`, `array_has`, `array_length`, `date_trunc`, `abs`, called without `DISTINCT`, `FILTER`, `OVER`, `ORDER BY` or named arguments.
- **Score functions:** `score(name)`, `score_label(name)` and `has_score(name)` on `traces`, `spans` and `sessions` only, each with a single string-literal argument (D97). For each target they use the latest live score of that name (greatest `updated_at`, then greatest `score_id`). `score` is its `value_num` (null for categorical and text scores), `score_label` its `value_str`, `has_score` is true when such a score exists. Each distinct name compiles to a left join on a latest-score subquery. At most 8 distinct score names per filter.
- **Limits:** at most 4 KiB and 128 operator tokens (D95). An operator token is one of `=`, `==`, `<>`/`!=`, `<`, `>`, `<=`, `>=`, `<=>`, `+`, `-`, `*`, `/`, `%`, integer division, `||`, `@>`, `<@`, `->`, `->>`, `#>`, `#>>`, `@@`, `?`, `?&`, `?|`, `&`, `|`, `^`, `<<`, `>>`, `~`, `~*`, `!~`, `!~*`, `::`, or a word whose keyword is `AND`, `OR`, `NOT`, `IS`, `IN`, `BETWEEN`, `LIKE`, `ILIKE`, `SIMILAR`, `XOR` or `DIV`.
- Anything else (subqueries, casts to other types, window functions, unknown identifiers/functions) → 400 `invalid_filter` with a 1-based character position when one is known.
- `duration_ms` is available as a derived column (`duration_ns / 1e6`) on the `spans`, `traces` and `sessions` views.

A filter can be checked without running for the traces, spans, sessions and scores views; the SQL console's statement can be planned without running (`sql_check`), which the assistant's drafts use (D-AS-6).

### UI builder

The filter text is the truth (D129, spec 11 § Filter bar). One editor holds the DSL. A client parser splits top-level `AND` conjuncts and recognises simple shapes as editable chips; anything else is a raw chip. The server stays the validator.

### Saved views

All list-page state lives in URL search params; a **saved view** stores those params under a name (`saved_views`, 04): private or shared with the project, with an optional default per page. The SQL console saves queries the same way (`kind = 'sql'`, state `{query, params, range | from+to}`, D-H-7). Visibility and who may share, edit or moderate: D-H-6. Saved views are also exposed to the MCP server (`list_saved_views`, and `search_traces` accepts `view`).

## Full-text search

The search box on list pages adds a conjunct:

```
(input ILIKE '%q%' OR output ILIKE '%q%' OR name ILIKE '%q%'
 OR system_instructions ILIKE '%q%')          -- spans view
```

The search text is at most 1,024 bytes; a longer one is a `400` (D-G-39).

`\` becomes `\\`, `%` becomes `\%` and `_` becomes `\_`. The pattern is `%<escaped>%`, with `ILIKE … ESCAPE '\'`. An empty or all-whitespace search adds nothing.

On the `traces` view, search runs on `spans` first (semi-join: `trace_id IN (SELECT trace_id FROM spans WHERE <search> AND <time window>)`). Search **requires a time window** (default: the page's time picker, max 30 days unless the user confirms "search all") because it is a scan. `cetra-query`'s `ILIKE` evaluates directly across `Flat`, `Dict`, `Const`, and `Rle` `Utf8` columns with `Selection`-vector late materialization, so only the needed heavy columns are read.

**On the live trace stream** (`GET /traces/stream`, 09), `search` is checked against each arriving span, on the same columns with the same escaping as the `spans`-view search. The stream sends a trace from its first matching span on, and every later update of that trace while the stream stays open. Spans that arrived before the stream opened are not checked; the client's refetch on `resync` covers them. The check runs as the `traces`-view search with its window set to the arriving spans' `start_time` range (narrowed to its last 24 h), so only spans in that range can match (D-C-5, D-G-33).

Later (post-MVP): Tantivy index per partition built at compaction.

## SQL console

`POST /api/v1/projects/{p}/sql` with `{query, params?, from?, to?, limit?, timeout_ms?}` (09).

Each run and each export writes an audit record before the query runs: it holds a SHA-256 of the query text, never the text. If the record cannot be written, the query does not run. A demo visitor's run is not recorded (D-EA-6, D-EA-23).

### Sandbox

1. Limits: 64 KiB and 512 operator tokens (the same operator tokens as the filter DSL; set operations `UNION`, `INTERSECT`, `EXCEPT`, `MINUS`, the keywords `AT`, `COLLATE`, `RLIKE`, `REGEXP`, `MATCH`, `MEMBER`, `OVERLAPS`, `OPERATOR`, subscripts `[`, `:`, `#` and the PostgreSQL operators `~~`, `~~*`, `!~~`, `!~~*`, `#-`, `@?`, `^@` count too); parser recursion limit 50 (D95, D-A2-1). At most 32 tables per statement: every view, CTE reference, derived table and `UNNEST` in a `FROM` or `JOIN` counts, wherever it appears, and so does every `INTERSECT` or `EXCEPT`, which runs as a join (D-A2-1).
2. Exactly one statement: a query (`SELECT`, `WITH`, `UNION`/`INTERSECT`/`EXCEPT`, `VALUES`), or `EXPLAIN` of a query without `ANALYZE` or `VERBOSE`. Pipe syntax (`|>`) is rejected (D-A2-1).
3. Table references are unqualified names of the registered views (`spans`, `traces`, `sessions`, `scores`, `experiment_items`, `dataset_items`) or of CTEs in scope. No table functions, no `information_schema`, no URL tables, no recursive CTEs.
4. Functions: Shape B SQL (`D-QE-4`) provides an explicit curated allow-list of scalar, nested, aggregate, and window functions (~50 scalar functions; `COUNT`, `COUNT(DISTINCT ...)`, `SUM`, `AVG`, `MIN`, `MAX`, `approx_percentile_cont` via bounded-memory DDSketch, `percentile_cont` gated by `ScopedPool`, `BOOL_OR`, `BOOL_AND`, `ARRAY_AGG`, `FILTER (WHERE ...)`; and `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, and running aggregates over `PARTITION BY` / `ORDER BY` with default or `ROWS BETWEEN` frames). Functions whose result can be far larger than their arguments or that read storage internals are explicitly denied: `range`, `generate_series`, `repeat`, `lpad`, `rpad`, `replace`, `regexp_replace`, `array_repeat`, `array_resize`, `array_replace`, `array_replace_n`, `array_replace_all` (and their `list_*` aliases), `levenshtein` (quadratic CPU), `input_file_name` and `file_row_index` (`DENIED_FUNCTIONS`, D-A2-2). Using a denied or unsupported function is a 400 naming it. Correlated subqueries, `WITH RECURSIVE`, `LATERAL`, `ROLLUP`/`CUBE`/`GROUPING SETS`, and `GROUPS`/arbitrary `RANGE` window frames are rejected with `400 invalid_filter`. Every operator and expression allocation reserves bytes from the query's `ScopedPool` (`GlobalPool` / `FairSpill`, capped at `query.max_memory`); exceeding the pool fails with `422 query_too_large`. No table or higher-order functions. Score functions are DSL-only (D97).
5. Read-only enforcement: the sandbox validator (`cetra_query::sql::validate`) accepts only single-statement queries and plain `EXPLAIN` (returning `["plan_type", "plan"]` with GDPR `<n redacted>` redaction on erased user/session tombstone lists, spec 03 §7), rejecting DDL, DML, and session/utility statements.
6. Execution config: per-query `ScopedPool` from storage (memory cap), `DEFAULT_BATCH_SIZE = 8192`, `target_partitions` on `WorkerPool`, `SpillManager` under `tmp/query/`. Timeout: `tokio::time::timeout` around planning and execution; dropping the stream or future triggers `CancelToken` and drains workers before releasing reservations.
7. Time window (D101): an optional `[from, to)` written into the `spans` and `traces` views (on `start_time`) and the `scores` view (on `created_at`); `sessions` follows from `traces`; the SQLite views are not windowed. Every scan of a view carries it, in joins, subqueries and CTEs too. Without a window the query reads all history. Independently of it, an org's plan data-access window (D175) hides older rows from every base view through `QueryOptions::visible_from`, which the server sets for every read (D-P-21, D-P-47).
8. Results (D98): `{columns: [{name, type}], rows: [{column: value}], truncated}`. Output column names must be unique. Timestamps are RFC 3339 with 9 fractional digits, `Z` for UTC (`Z` omitted for naive `CAST(... AS TIMESTAMP)`, D64). Binary and `FixedBytes8`/`FixedBytes16` are lowercase hex, `Decimal128` is a decimal string, `Date` is `YYYY-MM-DD`, `Interval` is canonical SQL interval text, maps are objects, `Events`/`Links` are JSON arrays, NaN and ±inf are null. `columns[].type` uses the unified cetra display vocabulary (`string`, `integer`, `float`, `decimal`, `boolean`, `timestamp`, `date`, `interval`, `binary`, `string_list`, `string_map`, `list`, `json`, `null`, D-QE-15). At most 10,000 rows (`limit` 1–10,000, default 10,000). `truncated: true` means more rows matched than `rows` holds, because of `limit` or the byte cap. The encoded JSON answer is at most 32 MiB (`SQL_MAX_RESULT_BYTES`): rows are pulled while their estimate, plus the answer's brackets and a comma per row, fits, and the answer stops at the last row that fits. Only a first row that alone passes the cap is `TooLarge`. Each row is sized as the JSON text its values encode to: a string's bytes, a binary's hex length (two characters a byte), a list or map's children in the rows it reaches (a selection/slice does not count unselected rows), a dictionary value by its key, a run-end value by its run, 40 bytes per number, bool or temporal value, a decimal by its precision and scale (`max(precision, scale) + 3` for a positive scale, `precision + |scale| + 1` otherwise), a calendar interval by its display bound (96 bytes), `null` as 4 bytes, a null dictionary slot as `null` whatever its key, and each cell's column name and punctuation (name length + 4). Each alias is a cell, so it counts its values again. The estimate does not count JSON escapes, so it can read low for control characters and quotes; the encoder refuses output past the cap or past the query memory pool, and either is `TooLarge`, answered `422 query_too_large`. A row whose estimate would pass the cap stops the answer instead (`truncated`); the encoder's own refusal is only reached by escapes the estimate does not count. CSV, NDJSON and Parquet exports stream up to 10M rows and stop there. Each CSV/NDJSON export batch is sliced into pieces of at most 8 MiB estimated size (at least one row), and each encoded chunk is at most 64 MiB (`EXPORT_MAX_CHUNK_BYTES`); a single row over that ends the stream as `TooLarge`. Parquet exports stream via `cetra-storage::engine::parquet_export` using `parquet::file::writer::SerializedFileWriter` (ZSTD level 1, 32 MiB / 1M-row row groups gated by `Reservation`, no `arrow` feature, D-QE-12). A failure after the first byte aborts the response body (D-H-8). In CSV exports, a string cell or header name whose first character is `=`, `+`, `-`, `@`, tab or CR gets a leading `'`; numbers, bools, nulls and JSON arrays and objects are unchanged, and NDJSON and Parquet are exact (D-A2-5).

### Parameters

`{name:Type}` placeholders, `Type` ∈ `String`, `Int`, `Float`, `Bool`, `Timestamp` (case-insensitive), are rewritten to positional `$n` placeholders before parsing and bound to typed `ScalarValue`s in `cetra-query` (D99). Values arrive as JSON: string, integer, number, boolean; `Timestamp` takes RFC 3339 or integer epoch milliseconds. A missing, extra or ill-typed parameter is a 400 that names it. A placeholder may appear several times and binds one value. Placeholders are only recognized outside string literals and comments. Writing `$1` directly is rejected (400). Used by the UI's saved queries and the MCP tool.

### Examples shipped in the UI

- Cost per model per day.
- p50/p95/p99 latency of LLM spans per model.
- Traces with the most tool calls.
- Average judge score per release.
- Users by spend.

## Dashboard queries

Every dashboard chart, built-in or custom, is a **widget query** computed by one endpoint (D-C-16). `POST /api/v1/projects/{p}/dashboards/query` reads only, so a read-only PAT and a `read` key may call it, as for `POST /sql`.

Request body:

| Field | Type | Notes |
|---|---|---|
| `from`, `to` | RFC 3339 | required, `from < to` |
| `bucket` | `auto` or a width | as below; ignored for `shape: total` |
| `filter` | traces DSL | the dashboard's filter, optional |
| `group_by` | `model` \| `environment` \| `release` | the dashboard's split, optional |
| `include_eval`, `include_experiments` | bool | default `false` (§ Default exclusions) |
| `query` | WidgetQuery | below |
| `shape` | `series` \| `total` | `series` = one value per bucket; `total` = one value over `[from, to)` |
| `compare` | bool | `total` only (ignored for `series`): also the value over the previous range of the same length |

**WidgetQuery** `{source, filter?, measures, breakdown?}`:

- `source`: `traces`, `spans` or `scores`. The rows are those of the view with that name (§ Logical views); scores are the live ones, and session-target scores are left out.
- `filter`: a DSL expression on the source view.
- `measures`: 1–6 items `{agg, field?, where?}`.
  - `agg`: `count`, `count_distinct`, `sum`, `avg`, `min`, `max`, `p50`, `p75`, `p90`, `p95`, `p99` or `rate`.
  - `field`: none for `count` and `rate`; a numeric field for `sum`, `avg`, `min`, `max` and the percentiles; any breakdown field except the list fields `tags` and `models`, or `trace_id`, `session_id` or `user_id`, for `count_distinct`.
  - `where`: a DSL expression on the source view. For `rate` it is required and the value is the share of the rows that match it (0 to 1). For every other aggregate it is optional and keeps only the matching rows for that measure. A `where` cannot call the score functions (`score`, `score_label`, `has_score`); the widget filter can.
- `breakdown`: `{field, limit?, other?}`. `limit` is 1–20 (default 5): the groups kept are the `limit` values with the largest first measure over `[from, to)`, nulls last, then by value ascending. With `other` (default `true`) the rows of every other value form one more group, `other: true`, whose measures are computed over those rows (not summed from the groups). A list field (`tags`, `models`) counts a row once under each of its values. Rows whose value is null are left out.

Fields by source:

| Source | Numeric fields | Breakdown fields |
|---|---|---|
| traces | `duration_ns`, `span_count`, `error_count`, `llm_count`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens`, `total_tokens`, `cost_total` | `name`, `status`, `environment`, `release`, `user_id`, `session_id`, `tags`, `models`, `metadata['k']` |
| spans | `duration_ns`, `ttft_ns`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens`, `total_tokens`, `cost_input`, `cost_output`, `cost_total` | `name`, `span_kind`, `status`, `service_name`, `environment`, `release`, `provider`, `model`, `operation`, `user_id`, `tags`, `metadata['k']`, `attributes['k']`, `resource_attributes['k']` |
| scores | `value_num` | `name`, `source`, `data_type`, `target`, `value_str`, `metadata['k']` |

A map key is 1–256 characters, written as in the DSL. Any other field, or a `field` that its `agg` does not take, is `400 bad_request` with the field's path in `errors` (e.g. `query.measures[1].field`).

**Time and filters.** The time column is `start_time` for traces and spans and `created_at` for scores. The widget's `filter` and each measure's `where` apply to the source view. The dashboard's `filter` is a traces expression: it applies to traces directly, and to spans and scores through a semi-join on `trace_id` over traces with `start_time ∈ [from − 1 day, to)` and `end_time ≥ from`. The default exclusions apply as for the lists; "the filter explicitly constrains `environment`" looks at the dashboard filter, the widget filter and every `where`. Each expression follows the DSL limits; an invalid one is `400 invalid_filter` naming its path (`filter`, `query.filter`, `query.measures[i].where`) with a position.

**Group by.** `group_by` acts as a breakdown with `limit: 20, other: false` on the traces field `models`, `environment` or `release` (spans: `model`, `environment`, `release`). It is ignored when the query has its own breakdown, and for scores.

**Buckets** (`shape: series`). `bucket=auto` picks the smallest width in `1m, 5m, 10m, 15m, 30m, 1h, 6h, 12h, 1d` giving at most 120 buckets over `[from, to)`, else `1d`; past 2,000 daily buckets (aligned to midnight UTC, so a 2,000-day window that does not start at midnight already has 2,001) it is a 400, as for an explicit bucket (D-A2-3). Buckets are aligned to their width from the Unix epoch, UTC: a 10-minute bucket starts at :00, :10, …; a 1-day bucket at midnight UTC. The first bucket starts at `from` rounded down to the width, so it may cover part of its range only; the last one is the bucket containing `to − 1 ns`. An explicit bucket giving more than 2,000 buckets is a 400. Rows are bucketed with `date_bin` over the time column.

**Answer.** The answer carries `shape` (`series` or `total`) next to the fields below.

- `series`: `{bucket_ns, series: [{measure, group, other, points: [{time, value, rows}]}]}`. `measure` is the measure's index; `group` is the breakdown value (null without a breakdown, and for the other group); `rows` is the number of source rows of the point's group in the bucket. A bucket with no rows for a series has no point; a point whose aggregate is null (e.g. a percentile over null values) has no `value`. Series come in breakdown order (largest first, other last), then measure order.
- `total`: `{rows: [{group, other, values, previous?}]}`, one row per group in the same order (one row without a breakdown). `values[i]` is measure `i`, null when the group has no rows (a `count` whose `where` matches none of the group's rows is 0); `previous` is there with `compare`, over `[from − (to − from), from)`.

Counts are integers; `rate` is 0 to 1; durations are nanoseconds; costs are summed as `Decimal128(38, 18)` and returned as numbers (D-A2-4). A non-finite or out-of-range trace cost fails the query with `query_failed`; it is not skipped.

**Cost and caching.** A non-breakdown widget query runs in **one shared scan** of the window (plus a second scan of `[from − (to − from), from)` when `shape: total` sets `compare: true`, and the traces semi-join when a dashboard `filter` applies to `spans` or `scores`). A breakdown widget runs in **two bounded scans** per range: pass 1 ranks the top-`limit` groups by the first measure; pass 2 computes all measures across buckets for those ≤20 groups plus `other` in one shared scan (D-QE-5, D-QE-13). A query has the list reads' timeout (`422 query_timeout`). Answers are cached for 10 s per `(project, body)`, counted from the moment the query finishes. Concurrent requests for the same key share one query, and a failed query's error goes to every request waiting on it; nothing is cached for it.

## Facets

`POST /api/v1/projects/{p}/facets` counts the values of 1 to 20 fields over the rows a list would show (window, default exclusions, `search`, `filter`) for the filter sidebar, value autocomplete and presets (D-C-13, D-C-24). It is a read by POST (D-H-4). The body gives what the fields share once: `view` (`traces`, `spans` or `sessions`), `from`, `to`, `filter`, `search`, `include_eval`, `include_experiments`; then `fields: [{ field, limit?, q? }]`. The answer is `{ view, results }`, with one result per field in request order (a field asked twice is answered twice): `{ field, facet }`, or `{ field, error }` where `error` is a problem document. A bad field, a `limit` outside 1–100 or a bad `q` (`400 bad_request`) and a field that does not finish before the deadline (`query_timeout`) fail only that field. A malformed body, a bad window, a `search` over 1,024 bytes, `search` on sessions, 0 or more than 20 fields (`400 bad_request`), a bad filter (`400 invalid_filter`, field `filter`) and a timeout while building the rows fail the whole request. The body is at most 64 KiB. A `limit` that is not a whole number from 0 to 2³²−1 is a malformed body. Storage evaluates the list's filter and updates all valid requested fields together in one shared scan (Pass 1), followed by at most one narrow Pass 2 scan for histogram buckets, under one deadline, the list reads' timeout (with per-field `TooLarge` and Pass 2 `Timeout` isolation, D-QE-5).

`field` is a facet column of the view, or `map['key']` for a `string_map` column (key 1–256 characters, written as in the DSL). A bare map column facets its keys; keys starting with `cetra.` are left out. Facet columns:

| View | values | list values | keys | histogram |
|---|---|---|---|---|
| traces | `name`, `status`, `environment`, `release`, `session_id`, `user_id`, `has_root` | `tags`, `models`, `prompts` | `metadata` | `duration_ms`, `cost_total`, `total_tokens`, `input_tokens`, `output_tokens`, `span_count`, `error_count`, `llm_count` |
| spans | `name`, `span_kind`, `status`, `service_name`, `environment`, `release`, `provider`, `model`, `request_model`, `response_model`, `prompt_name`, `operation`, `cost_source`, `scope_name`, `session_id`, `user_id` | `tags` | `metadata`, `attributes`, `resource_attributes` | `duration_ms`, `cost_total`, `total_tokens`, `input_tokens`, `output_tokens` |
| sessions | `user_id` | `tags` | — | `duration_ms`, `trace_count`, `error_count`, `total_tokens`, `cost_total` |

Any other field is a `bad_request` error for that field; `search` on sessions is `400 bad_request` for the request. IDs other than session and user IDs are not facets (their cardinality is the row count). A trace filter `prompts @> ['support-bot@5']` selects the traces that link that prompt version (D-V-15).

The answer has `kind` (`values`, `keys` or `histogram`), `total` (rows counted) and `missing` (rows with no value: null, or an empty list or map; for numbers also NaN and ±inf). `values` and `keys` return at most `limit` (1–100, default 10) values with their row counts, by count descending then value ascending, and `more` when more matched; `q` (1–200 characters) keeps values containing it, case-insensitively. A list row counts once under each distinct element, a map row once under each key; booleans are `"true"`/`"false"`. `histogram` returns exact `min` and `max`, approximate `p50`, `p95` and `p99`, and buckets: 20 of equal width over `[min, p99)` plus `[p99, max]`, or one `[min, max]` bucket when `p99 ≤ min`; it is absent when no row has a value. Values are `f64`.

The spans list drops stored copies of a span and facets do not, so a span stored twice before compaction counts twice. The users list filters traces, so its facets count traces.

Cost: there are no pre-aggregates. Pass 1 evaluates the filter once and updates all valid requested `values`, `list values`, `map keys` and histogram summary accumulators (`min`, `max`, `p50`, `p95`, `p99`) in **one shared scan** (high-cardinality `values` facets use spillable `HashAgg`); Pass 2 runs at most one extra narrow scan over the requested histogram numeric columns once `[min, p99)` is known — at most 2 scans total (D-QE-5). Answers are not cached on the server; the UI fetches only expanded facets, keeps each answer 30 s, and sends the fields it asks together (within 10 ms, with the same window, flags, filter and search) as one request (D-C-24).

## Trace and span APIs

These queries are `cetra_storage::reads` (D92).

| Operation | Plan |
|---|---|
| Trace list | `traces` view, filter, sort `start_time DESC` (or by `cost_total`, `duration`, `total_tokens`), cursor `(sort_value, trace_id)`; fast path from 03 §6 |
| Trace detail | `spans WHERE trace_id = X`, light columns only for the tree; heavy columns fetched per span on demand (`GET …/spans/{span_id}`) |
| Session detail | `traces WHERE session_id = X ORDER BY start_time`, then root input/output per trace for the chat view |
| Span list | `spans` view with filter (e.g. all LLM calls for model X) |
| User list | `SELECT user_id, count(*), sum(cost_total), max(end_time) … FROM traces GROUP BY user_id` |

Sort tie-breakers: traces `trace_id`; spans `(span_id, trace_id)`; sessions `session_id`; users `user_id`; scores `score_id`, with lists sorted by `created_at`. Pagination is keyset on `(sort value, tie-breaker)`, nulls last in both directions.

Trace tree assembly happens in Rust after the scan (parent/child map); spans whose parent is missing, or that sit in a parent cycle, are emitted as roots with `detached: true`, in pre-order by `(start_time, span_id)` (09 § Response shapes).

## Errors

Filter, SQL and execution errors (D94) go back to the caller, capped at 2 KiB, and are never logged. Logs carry the error kind, the text lengths and the duration.

| Code | Status | When |
|---|---|---|
| `invalid_filter` | 400 | Filter and SQL text, with `position` when one is known |
| `query_failed` | 400 | Execution errors (bad cast, regex too big, division by zero, integer overflow, non-finite cost), with `cetra-query`'s message capped at 2 KiB |
| `query_timeout` | 422 | The query exceeded its time limit |
| `query_too_large` | 422 | The query needed more memory or temporary disk space than the cap |
| `busy` | 503 | Admission control refused the query |
