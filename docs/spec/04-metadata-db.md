# 04 — Metadata database (SQLite)

`meta.db`, SQLite ≥ 3.45, `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`, `busy_timeout=5000`. Writes are rare; every commit is durable before the response (D136). Accessed through `sqlx` with one writer connection (serialized writes through a `Mutex`) and a read pool of 4 read-only connections. Migrations are embedded (`sqlx::migrate!`) and run in `cetra serve`; downgrade is not supported (12 §Upgrades). `cetra admin` commands open `meta.db` without running migrations and refuse a database at another migration version (D136). A `meta.db` of a newer format or with a migration the binary lacks is refused before anything is written (§ Format versions).

`meta.db` belongs to `cetra-server`. The storage engine keeps its own catalog in `catalog.db` (03 §4, D83): data files, WAL checkpoints, media index and its side of deletions. The two databases share no transaction; the only link is the deletion handoff (§ Deletion requests).

The SQL is written in the common subset of SQLite and Postgres where practical (no `AUTOINCREMENT` except where noted, `TEXT` UUIDs, timestamps as `INTEGER` ns), to keep the later Postgres port mechanical.

Conventions: `id TEXT PRIMARY KEY` (UUIDv7), `created_at`/`updated_at INTEGER` (ns), JSON columns are `TEXT` validated by the application.

## Identity & tenancy

```sql
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,         -- normalized: trimmed, NFC, lowercase (D136)
  name TEXT,
  password_hash TEXT,                 -- argon2id PHC string; NULL for SSO-only users
  email_verified_at INTEGER,
  email_verified_via TEXT,            -- 'mail_token' | 'idp' | 'invite' | 'control_plane' | 'operator' (D-P-66)
  is_instance_admin INTEGER NOT NULL DEFAULT 0,   -- first user; can manage instance settings
  disabled_at INTEGER,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

CREATE TABLE identities (            -- linked OAuth/OIDC accounts
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,             -- 'github' | 'google' | 'oidc:<name>'
  subject TEXT NOT NULL,
  email TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE(provider, subject)
);

CREATE TABLE web_sessions (
  id TEXT PRIMARY KEY,                -- SHA-256 hex of the cookie value (32 random bytes, base64url)
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
  user_agent TEXT, ip TEXT,
  sso_org_id TEXT REFERENCES orgs(id) ON DELETE SET NULL, -- the org whose provider started it (D-P-59, track P M5)
  via_provider INTEGER NOT NULL DEFAULT 0  -- 1 when any identity provider started it (D-P-69)
);

CREATE TABLE orgs (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
  state TEXT NOT NULL DEFAULT 'active', -- 'active' | 'deleting'
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

CREATE TABLE org_members (
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role TEXT NOT NULL,                 -- 'owner' | 'admin' | 'member' | 'viewer'
  created_at INTEGER NOT NULL,
  PRIMARY KEY (org_id, user_id)
);

CREATE TABLE projects (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  name TEXT NOT NULL, slug TEXT NOT NULL,
  retention_days INTEGER,             -- NULL = forever
  settings TEXT NOT NULL DEFAULT '{}',-- JSON: trace_completion_timeout_s (integer seconds, 10–3600; absent means 60, D-I-7), media settings, …
  state TEXT NOT NULL DEFAULT 'active',
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  UNIQUE(org_id, slug)
);

CREATE TABLE project_members (        -- optional per-project role override
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role TEXT NOT NULL,                 -- 'admin' | 'member' | 'viewer' | 'none'
  PRIMARY KEY (project_id, user_id)
);

CREATE TABLE invites (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  email TEXT NOT NULL,                -- normalized: trimmed, NFC, lowercase (D136)
  role TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  invited_by TEXT REFERENCES users(id),
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, accepted_at INTEGER
);
CREATE UNIQUE INDEX invites_pending ON invites(org_id, email) WHERE accepted_at IS NULL;

CREATE TABLE password_resets (
  token_hash TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, used_at INTEGER
);

CREATE TABLE email_verifications (
  token_hash TEXT PRIMARY KEY,        -- SHA-256 hex of a 32-byte random token
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, used_at INTEGER
);
```

A new invite for the same (org, email) replaces the pending one (D147).

## Keys & credentials

```sql
CREATE TABLE api_keys (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,                 -- 'public' | 'secret'
  scopes TEXT NOT NULL,               -- JSON array: ['ingest'] | ['ingest','read','write'] | ['feedback']
  name TEXT,
  prefix TEXT NOT NULL,               -- type prefix + first 6 random characters, e.g. `sk-cetra-a1B2c3` (D140)
  key_hash TEXT NOT NULL UNIQUE,      -- SHA-256 hex
  created_by TEXT REFERENCES users(id),
  created_at INTEGER NOT NULL, expires_at INTEGER, last_used_at INTEGER, revoked_at INTEGER
);

CREATE TABLE personal_tokens (        -- for CLI / MCP; act as the user, with the user's roles
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT,
  prefix TEXT NOT NULL,               -- type prefix + first 6 random characters, e.g. `pat-cetra-a1B2c3` (D140)
  token_hash TEXT NOT NULL UNIQUE,
  scopes TEXT NOT NULL,               -- ['read'] | ['read','write']
  created_at INTEGER NOT NULL, expires_at INTEGER, last_used_at INTEGER, revoked_at INTEGER
);

CREATE TABLE llm_connections (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  adapter TEXT NOT NULL,
  base_url TEXT,
  secret_enc BLOB NOT NULL,
  api_key_hint TEXT NOT NULL,
  extra_headers_enc BLOB,
  extra_header_names TEXT NOT NULL DEFAULT '[]',
  default_model TEXT,
  structured_output TEXT NOT NULL DEFAULT 'native',
  max_concurrency INTEGER NOT NULL DEFAULT 4,
  requests_per_minute INTEGER NOT NULL DEFAULT 300,
  status TEXT NOT NULL DEFAULT 'ok',
  last_error TEXT,
  last_error_at INTEGER,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(org_id, name)
);
```

Connections belong to the org (D-I-4). `secret_enc` and `extra_headers_enc` are sealed (§ Encryption of secrets); `api_key_hint` and `extra_header_names` are what the API shows.

## Deletion requests

The storage catalog (data files, WAL checkpoints, media) is in `catalog.db` (03 §4). `meta.db` keeps the deletion requests (D-M-3). The API creates a row; a background worker then resolves its scope (the traces of a filter or of an end user, plus their evaluation traces), stores it here together with the `meta.db` cascades (03 §7), hands it to storage (`register_deletion`, idempotent by id) and sets `handed_over_at`, then polls `deletion_status` until it is applied. Project and org drops are created `pending` and run `drop_project` before their rows are deleted. At startup the worker picks up every open row. Rows and their scope are kept forever, with no foreign keys to `orgs` or `projects`, as the audit trail of erasure requests and so `cetra admin reapply-deletions` can redo them after a restore (D-M-8, D-M-9).

```sql
CREATE TABLE deletions (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL,
    project_id TEXT,
    kind TEXT NOT NULL,                 -- 'trace_ids' | 'filter' | 'user' | 'project' | 'org'
    user_id TEXT,
    request TEXT NOT NULL,              -- JSON: the filter selection, or the dropped project or org name and slug
    requested_by_kind TEXT NOT NULL,    -- 'user' | 'api_key'
    requested_by TEXT NOT NULL,
    status TEXT NOT NULL,               -- 'resolving' | 'pending' | 'applied' | 'failed'
    trace_count INTEGER,                -- NULL until the scope is stored
    session_count INTEGER,
    created_at INTEGER NOT NULL,        -- the cut-off, ns
    handed_over_at INTEGER,
    applied_at INTEGER,
    error TEXT                          -- 'too_many_traces' | 'resolution_failed'; NULL unless failed
);

CREATE INDEX deletions_project ON deletions(project_id, created_at, id);

CREATE INDEX deletions_org ON deletions(org_id, created_at, id);

CREATE INDEX deletions_open ON deletions(created_at, id) WHERE status IN ('resolving', 'pending');

CREATE TABLE deletion_traces (
    deletion_id TEXT NOT NULL REFERENCES deletions(id) ON DELETE CASCADE,
    trace_id BLOB NOT NULL,
    PRIMARY KEY (deletion_id, trace_id)
);

CREATE TABLE deletion_sessions (
    deletion_id TEXT NOT NULL REFERENCES deletions(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    PRIMARY KEY (deletion_id, session_id)
);

CREATE TABLE deletion_projects (
    deletion_id TEXT NOT NULL REFERENCES deletions(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL,
    PRIMARY KEY (deletion_id, project_id)
);
```

## Pricing

Custom prices of an org (`project_id` NULL, applies to every project of the org) or of one project (D-T-2). The built-in snapshot is not stored here: it stays embedded and is compiled once at startup (spec 02 § Cost computation).

```sql
CREATE TABLE model_prices (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,  -- NULL: an org row
  model_name TEXT NOT NULL,           -- display name
  match_pattern TEXT NOT NULL,        -- regex, anchored, case-insensitive
  provider TEXT,
  valid_from INTEGER NOT NULL DEFAULT 0,
  price_input REAL, price_output REAL, price_cache_read REAL, price_cache_write REAL, price_reasoning REAL,
  tiers TEXT,                         -- JSON [{min_input_tokens, price_input, …}]
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX model_prices_key ON model_prices(
  org_id, coalesce(project_id, ''), match_pattern, coalesce(provider, ''), valid_from
);
CREATE INDEX model_prices_project ON model_prices(project_id) WHERE project_id IS NOT NULL;
```

At most 500 org rows per org and 500 rows per project (D-T-4). The server loads every row at startup and keeps one compiled table per org and per project that has rows; each write reloads its org (D-T-9). A write that would take away the price a budgeted online rule's judge model has is refused (D-T-8).

## Scores definitions

```sql
CREATE TABLE score_definitions (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  data_type TEXT NOT NULL,            -- numeric | boolean | categorical | text
  categories TEXT,                    -- JSON [{label, value?}]
  min_value REAL, max_value REAL,
  description TEXT,
  direction TEXT NOT NULL DEFAULT 'none',  -- higher | lower | none
  archived_at INTEGER,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX score_definitions_live ON score_definitions(project_id, name) WHERE archived_at IS NULL;
```

`DELETE` archives; a name is unique among live definitions (D-I-26).

## Saved views

```sql
CREATE TABLE saved_views (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,                 -- 'traces' | 'spans' | 'sessions' | 'users' | 'sql'
  name TEXT NOT NULL,
  state TEXT NOT NULL,                -- JSON owned by the UI: the list page's search params (range|from+to, filter, search, sort, order, eval, exp, cols) or {query, params, range|from+to} for SQL (D-H-7)
  owner_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
  shared INTEGER NOT NULL DEFAULT 0,  -- 1 = visible to the whole project
  is_default INTEGER NOT NULL DEFAULT 0,  -- one per (project, kind, owner) among private views, one per (project, kind) among shared views (D-H-6)
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE INDEX saved_views_lookup ON saved_views(project_id, kind);
CREATE INDEX saved_views_owner ON saved_views(project_id, owner_user_id);   -- the per-owner cap (D-H-6)
```

Names are not unique: several visible views may share a name. Visibility, sharing, moderation, the two default scopes and the limits (names 1–200 characters, state ≤ 64 KiB, 1,000 views per project and owner) are D-H-6. Preset ranges are stored relative (`range: "24h"`). A view whose filter no longer validates (column removed) still loads; the filter shows as a raw chip with the error.

## Dashboards

Custom dashboards (D-C-16) and each project's edited Overview (D-C-18). The built-in Overview preset is defined by the UI; a project stores its own Overview only once a member saves a change to it.

```sql
CREATE TABLE dashboards (
  id TEXT PRIMARY KEY,                -- UUIDv7
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  description TEXT,
  definition TEXT NOT NULL,           -- JSON: {widgets: [Widget]}, validated by the server
  version INTEGER NOT NULL DEFAULT 1, -- +1 on every update; a PATCH names the version it read
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  updated_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  kind TEXT NOT NULL DEFAULT 'custom' CHECK (kind IN ('custom', 'overview'))  -- added by migration 0022 (D-C-18)
);
CREATE INDEX dashboards_project ON dashboards(project_id, name);
CREATE UNIQUE INDEX dashboards_overview ON dashboards(project_id) WHERE kind = 'overview';
```

A row of kind `overview` is the project's edited Overview: at most one per project, named `Overview`, with no description. It is read and written only through the `/dashboards/overview` endpoints (09): the list, the 100-dashboard cap and `/dashboards/{id}` ignore it (a `GET`, `PATCH` or `DELETE` of its id is a 404). Deleting it ("Reset to default") brings the built-in preset back.

A **Widget** is `{id, title, layout: {x, y, w, h}, kind, chart?, query?}`:
- `id`: 1–64 characters of `[A-Za-z0-9_-]`, unique in the dashboard.
- `title`: 0–100 characters (a heading needs 1–100). `description` of the dashboard: 0–500.
- `layout`: integers on the 60-column grid (D-C-19), `0 ≤ x ≤ 59`, `1 ≤ w ≤ 60`, `x + w ≤ 60`, `0 ≤ y ≤ 1000`, `1 ≤ h ≤ 40`.
- `kind`: `chart` or `heading`. A heading has no `chart` and no `query`.
- `chart`: `{type, stacked?, split?, fade_below?, share?}`. `type` is `line`, `bar`, `area`, `bar_h`, `number` or `table`. `stacked` applies to `bar` and `area`; `split` (one small chart per group; needs a breakdown with `limit` at most 6) to `line`; `fade_below` (1–1,000: buckets with fewer rows are faded) to `bar`; `share` (each row's share of the first measure's total, as a bar) to `table`.
- `query`: a WidgetQuery (05 § Dashboard queries), checked with the same rules, plus a `label` (0–60 characters) on each measure. `line`, `bar` and `area` read the `series` shape; `bar_h`, `number` and `table` read `total`. `bar_h` needs a breakdown; `number` has one measure and no breakdown.

Limits: names 1–100 characters (not unique), 50 widgets per dashboard, `definition` at most 256 KiB, 100 dashboards per project. An invalid definition is `400 bad_request` with the path of each error in `errors` (e.g. `widgets[3].query.measures[0].field`). A definition stored before a column was removed still loads; the widget's query answers its own 400.

## Annotation queues

Annotation queues (track AQ, D-AQ-2 to D-AQ-9): a queue of trace, span or session items that members review in order against an ordered list of score definitions. Each queue has one target kind, fixed at creation (D-AQ-2).

```sql
-- Annotation queues (track AQ, D-AQ-2–D-AQ-9).
CREATE TABLE annotation_queues (
  id TEXT PRIMARY KEY,                -- UUIDv7
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  description TEXT,
  instructions TEXT,                  -- Markdown
  target TEXT NOT NULL CHECK (target IN ('trace', 'span', 'session')),
  version INTEGER NOT NULL DEFAULT 1,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (project_id, name)
);

CREATE TABLE annotation_queue_definitions (
  queue_id TEXT NOT NULL REFERENCES annotation_queues(id) ON DELETE CASCADE,
  definition_id TEXT NOT NULL REFERENCES score_definitions(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  PRIMARY KEY (queue_id, definition_id)
);

CREATE TABLE annotation_queue_items (
  id TEXT PRIMARY KEY,                -- UUIDv7
  queue_id TEXT NOT NULL REFERENCES annotation_queues(id) ON DELETE CASCADE,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  trace_id TEXT,                      -- lowercase hex; trace and span items
  span_id TEXT,                       -- lowercase hex; span items
  session_id TEXT,                    -- session items
  target_time INTEGER NOT NULL,       -- ns: trace start, or the session's earliest visible trace start
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'completed', 'skipped')),
  skip_reason TEXT,
  leased_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
  lease_expires_at INTEGER,
  done_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
  done_at INTEGER,
  added_by_kind TEXT NOT NULL CHECK (added_by_kind IN ('user', 'pat', 'key')),
  added_by_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX annotation_queue_items_target ON annotation_queue_items(
  queue_id, coalesce(trace_id, ''), coalesce(span_id, ''), coalesce(session_id, '')
);
CREATE INDEX annotation_queue_items_claim ON annotation_queue_items(queue_id, status, id);
CREATE INDEX annotation_queue_items_trace ON annotation_queue_items(project_id, trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX annotation_queue_items_session ON annotation_queue_items(project_id, session_id) WHERE session_id IS NOT NULL;
CREATE INDEX annotation_queue_items_time ON annotation_queue_items(project_id, target_time);
```

One kind per queue (D-AQ-2). Items are unique per target within a queue. `target_time` drives retention and the data window (D-AQ-9). The lease is `leased_by_user_id` plus `lease_expires_at`: 5 minutes, claimed atomically on the writer (D-AQ-4). At most 100 queues per project, 100 000 items per queue, 50 definitions per queue.

## Assistant

Conversations of the in-app assistant (track AS, D-AS-10): private to their author in one project.

```sql
-- In-app assistant conversations (track AS, D-AS-10).
CREATE TABLE assistant_conversations (
  id TEXT PRIMARY KEY,                 -- UUIDv7
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  title TEXT NOT NULL DEFAULT '',      -- '' until the first user message (M3) or a rename
  message_count INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_active_at INTEGER NOT NULL
);
CREATE INDEX assistant_conversations_owner ON assistant_conversations(project_id, user_id, last_active_at);
CREATE INDEX assistant_conversations_idle ON assistant_conversations(last_active_at);
CREATE TABLE assistant_messages (
  conversation_id TEXT NOT NULL REFERENCES assistant_conversations(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,                -- 1, 2, … within the conversation
  role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool')),
  parts TEXT NOT NULL,                 -- JSON array of message parts (spec 04 § Assistant)
  context TEXT,                        -- JSON page context of a user message (D-AS-14)
  status TEXT NOT NULL DEFAULT 'complete' CHECK (status IN ('complete', 'cancelled', 'failed')),
  feedback TEXT CHECK (feedback IN ('up', 'down')),
  turn_trace_id TEXT,                  -- 32 lowercase hex characters
  stub_after INTEGER,                  -- tool messages: when their results become stubs; NULL once stubbed
  created_at INTEGER NOT NULL,
  PRIMARY KEY (conversation_id, seq)
);
CREATE INDEX assistant_messages_stub ON assistant_messages(stub_after) WHERE stub_after IS NOT NULL;
```

The org's assistant setting (D-AS-3) is one row per org; an org with no row has the assistant off.

```sql
-- The org's in-app assistant setting (track AS, D-AS-3).
CREATE TABLE org_assistant_settings (
  org_id TEXT PRIMARY KEY REFERENCES orgs(id) ON DELETE CASCADE,
  enabled INTEGER NOT NULL DEFAULT 0,
  connection_id TEXT REFERENCES llm_connections(id) ON DELETE SET NULL,
  model TEXT,
  updated_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  updated_at INTEGER NOT NULL
);
```

> Enabling needs a connection and a model; turning it on runs the tool-calling test (D-AS-24) and stores nothing when the test fails. The assistant never changes a connection's status. Deleting the connection leaves the setting with no connection, which reads as `no_connection`. A setting with `enabled` false, or with no row, reads as `disabled`.

> `parts` is a JSON array of parts. The message parts of spec 01 are used where they fit: `{type: "text", content}` and `{type: "tool_call", id, name, arguments}`. Two parts are the assistant's own: `{type: "tool_result", id, name, ok, result, size_bytes, rows?, stubbed}` (the tool's JSON answer, `null` once stubbed) and `{type: "draft", id, kind, draft}` (`kind` is `filter`, `sql`, `dashboard`, `evaluator` or `online_rule`; D-AS-6). A user message holds text parts and its page context in `context`; an assistant message holds text and tool calls; a tool message holds tool results, each followed by the draft it produced (D-AS-21, draft id `draft_<tool call id>`).
>
> Limits: 100 conversations per user and project (creating one more deletes the least recently active), 200 messages per conversation (enforced when turns append them, M3); a turn starts only when 34 more messages fit (D-AS-25). A conversation is deleted 30 days after `last_active_at`; tool results are stubbed from `stub_after`, 24 hours after their turn (D-AS-12). The expiry and the stubbing run at startup, then hourly, so a result can stay whole for up to about 25 hours. Conversations are deleted with their project, org or author.

## Datasets & experiments

Track J (D-J-1–D-J-19, [`plans/j-datasets/00-design.md`](../plans/j-datasets/00-design.md)). Dataset versioning uses validity ranges: an item revision is visible in version *v* iff `valid_from_version ≤ v < coalesce(valid_to_version, ∞)`. Every mutating request makes version `current_version + 1` with one `dataset_versions` row; editing an item closes the old revision and inserts a new one in the new version; a new dataset is at version 0 with no items (D-J-3). An experiment pins a dataset version and snapshots each item's input and expected output into its results, so later edits, item deletions and dataset deletions leave it intact (D-J-10). Experiments have no server evaluators (D-J-11).

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

Experiment scores are rows in the columnar `scores` table with `experiment_id` + `dataset_item_id` set (D-J-12). Per-item cost comes from the item's trace at read time (D-J-13). A trace deletion hard-deletes the dataset item revisions copied from the trace and the experiment results that point at it or snapshot such a revision (D-J-9).

## Prompts (track V)

Track V ([`plans/v-prompts/00-design.md`](../plans/v-prompts/00-design.md), D-V-2 to D-V-13). Prompts belong to a project and their names are immutable. Each prompt keeps its versions, its labels and the history of label moves; the settings row holds the project's protected labels.

```sql
-- Track V: the prompt registry (D-V-7, D-V-8, D-V-10, D-V-11, spec 04 § Prompts).
CREATE TABLE prompts (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,                   -- immutable (D-V-2)
  type TEXT NOT NULL CHECK (type IN ('text', 'chat')),
  description TEXT,
  tags TEXT NOT NULL DEFAULT '[]',      -- JSON array of strings
  latest_version INTEGER NOT NULL,
  created_by TEXT,                      -- a user id or key:<key id>
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);

CREATE TABLE prompt_versions (
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  template TEXT NOT NULL,               -- JSON: a string (text) or an array (chat)
  config TEXT NOT NULL DEFAULT '{}',    -- JSON object (D-V-6)
  commit_message TEXT,
  created_by TEXT,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (prompt_id, version)
);

CREATE TABLE prompt_labels (
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  label TEXT NOT NULL,
  version INTEGER NOT NULL,
  updated_by TEXT,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (prompt_id, label),
  FOREIGN KEY (prompt_id, version) REFERENCES prompt_versions(prompt_id, version)
);

CREATE TABLE prompt_label_events (
  id INTEGER PRIMARY KEY,               -- rowid: the history's order
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  label TEXT NOT NULL,
  from_version INTEGER,                 -- NULL: the label was new
  to_version INTEGER,                   -- NULL: the label was removed
  actor TEXT,
  at INTEGER NOT NULL
);
CREATE INDEX prompt_label_events_prompt ON prompt_label_events(prompt_id, id);

CREATE TABLE prompt_settings (
  project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  protected_labels TEXT NOT NULL,       -- JSON array; no row means ["production"] (D-V-10)
  updated_by TEXT,
  updated_at INTEGER NOT NULL
);
```

- Versions are immutable and numbered from 1 (D-V-7).
- `latest` is computed from `prompts.latest_version`, never stored as a label (D-V-8).
- Every label move, creation and removal writes a `prompt_label_events` row, so the history is complete (D-V-8).
- A project without a `prompt_settings` row protects `production` (D-V-10).

## Evaluation

```sql
CREATE TABLE evaluators (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,                 -- 'llm_judge' | 'check' | 'webhook'; immutable
  config TEXT NOT NULL,               -- JSON, schema per kind (06)
  output_score TEXT NOT NULL,         -- JSON {name, data_type, categories?}
  connection_id TEXT REFERENCES llm_connections(id),
  secret_enc BLOB,                    -- webhook: sealed {signing_secret, url, headers}
  version INTEGER NOT NULL DEFAULT 1,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);
CREATE INDEX evaluators_connection ON evaluators(connection_id) WHERE connection_id IS NOT NULL;

CREATE TABLE evaluator_versions (
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  config TEXT NOT NULL,               -- as evaluators.config: no sealed value
  output_score TEXT NOT NULL,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (evaluator_id, version)
);

CREATE TABLE online_rules (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id),
  name TEXT NOT NULL,
  target TEXT NOT NULL,
  filter TEXT,
  variable_mapping TEXT NOT NULL,
  sample_rate REAL NOT NULL DEFAULT 1.0,
  budget_usd REAL,
  budget_period TEXT,
  enabled INTEGER NOT NULL DEFAULT 1,
  paused_reason TEXT,
  paused_at INTEGER,
  paused_detail TEXT,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);
CREATE INDEX online_rules_evaluator ON online_rules(evaluator_id);

CREATE TABLE eval_jobs (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  priority INTEGER NOT NULL,
  rule_id TEXT REFERENCES online_rules(id) ON DELETE CASCADE,
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id) ON DELETE CASCADE,
  variable_mapping TEXT,
  trace_id BLOB NOT NULL,
  span_id BLOB,
  trace_start_ns INTEGER NOT NULL,
  dedup_key TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  claims INTEGER NOT NULL DEFAULT 0,
  load_retries INTEGER NOT NULL DEFAULT 0,
  interruptions INTEGER NOT NULL DEFAULT 0,
  not_before INTEGER NOT NULL,
  error_class TEXT,
  error TEXT,
  judge_trace_id BLOB,
  score_id TEXT,
  evaluator_version INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost_usd REAL,
  latency_ms INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX eval_jobs_ready ON eval_jobs(status, priority, not_before);
CREATE INDEX eval_jobs_rule ON eval_jobs(rule_id, created_at);
CREATE INDEX eval_jobs_project ON eval_jobs(project_id, created_at);
CREATE INDEX eval_jobs_finished ON eval_jobs(finished_at) WHERE finished_at IS NOT NULL;

CREATE TABLE rule_spend (
  rule_id TEXT NOT NULL REFERENCES online_rules(id) ON DELETE CASCADE,
  day INTEGER NOT NULL,
  spent_usd REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (rule_id, day)
);

CREATE TABLE pending_traces (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  trace_id BLOB NOT NULL,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  min_start INTEGER NOT NULL,
  root_ended INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, trace_id)
);

CREATE TABLE backfills (
  id TEXT PRIMARY KEY,
  rule_id TEXT NOT NULL REFERENCES online_rules(id) ON DELETE CASCADE,
  from_ns INTEGER NOT NULL,
  to_ns INTEGER NOT NULL,
  max_targets INTEGER NOT NULL,
  sample_rate REAL NOT NULL,
  status TEXT NOT NULL,
  matched INTEGER NOT NULL DEFAULT 0,
  enqueued INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE UNIQUE INDEX backfills_running ON backfills(rule_id) WHERE status = 'running';
CREATE INDEX backfills_rule ON backfills(rule_id, created_at);
```

Every create, and every update that changes `version`, writes one `evaluator_versions` row in the same transaction, copied from the stored row: name, kind, the redacted config, the output score, the author and the time. It holds no sealed value. A rename alone does not change `version`, so it writes no row. The history lives as long as the evaluator and is deleted with it (cascade); it has no retention cap and no per-version delete, so text removed from a prompt stays readable in it until the evaluator is deleted (D-I-49).

`pending_traces` is the persisted copy of the trigger's in-memory map (spec 06 § Online rules › Triggering, D-I-8): the flusher writes the changed entries every 5 s and at shutdown, and startup loads them. `first_seen` and `last_seen` are ingest times; `min_start` is the earliest span `start_time` seen.

`backfills` holds an online rule's runs over stored traces (spec 06 § Online rules › Backfill; D-I-25, D-I-39 to D-I-42). A backfill is `running` until the runner reads its last page; `matched` counts the targets read and `enqueued` the jobs inserted or re-queued. At most one backfill per rule is `running` (partial unique index). Startup marks the ones left `running` `failed` with `error = 'interrupted'`. Rows go with their rule.

`connection_id` is derived from a judge's config; `secret_enc` holds a webhook's sealed `{signing_secret, url, headers}`, and `config` then keeps `{url_host, timeout_ms, header_names}` (D-I-19).

`eval_jobs` is the durable queue (spec 06 § Online rules › Execution; D-I-11, D-I-12): `dedup_key` makes enqueues idempotent, `claims` fences a worker's finish, and there is no lease. Every hour, finished jobs older than 7 days are deleted, queued jobs older than 24 h (7 days for backfill) are skipped as `expired`, and jobs of inactive projects or orgs are deleted (D-I-28). `rule_spend` holds one row per rule and UTC day; a month is the sum of its days; rows older than 62 days are deleted.

### Coding-agent session contexts (D-CA-9)

```sql
-- Coding-agent session contexts (D-CA-9): the repository, branch and person a hook reported.
CREATE TABLE agent_contexts (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  agent TEXT NOT NULL,          -- claude-code | codex | gemini-cli | copilot
  session_id TEXT NOT NULL,
  repository TEXT,              -- canonical form (D-CA-4)
  branch TEXT,
  user_id TEXT,                 -- the hook's `user`, lowercased
  updated_at INTEGER NOT NULL,  -- ns since the Unix epoch, time of the last write
  PRIMARY KEY (project_id, agent, session_id)
);
CREATE INDEX agent_contexts_user ON agent_contexts(project_id, user_id) WHERE user_id IS NOT NULL;
CREATE INDEX agent_contexts_updated ON agent_contexts(project_id, updated_at);
```

Written by `POST /v1/agent-context` (last write wins; an unchanged post within an hour writes nothing), loaded into memory before the listeners open, read by ingest from memory only. An hourly sweep deletes rows older than 30 days or the project's retention when shorter; a project keeps at most 10,000 rows, the oldest evicted; a deletion by user deletes the user's rows. Only active projects are loaded and swept; rows of a paused project stay until it is active again or dropped.

## Issues

Track IC (D-IC-1, D-IC-3, D-IC-10; design in `docs/plans/ic-issue-clustering/00-design.md`). `issue_sources` holds the sources of a project. `kind` is `error`, `score`, `feedback` or `topic` (D-IC-1) and has no SQL CHECK, so kinds are added without a table rebuild (the Rust enum is the only gate). `enabled` is the switch and `paused_reason` the pause of the sweeper or of the connection; the sweeper and the LLM workers read only enabled, unpaused sources. Deleting a source deletes its issues and their events (D-IC-3), and its candidates and refits.

The LLM kinds carry a chat connection and model (candidate summaries, naming), an embedding connection, model and dimension (D-IC-5; fixed at create, changed only through the embedding dimension setter), a `sample_rate` and `daily_cap` (D-IC-1), an optional `budget_usd` per `budget_period` (`day` or `month`, D-IC-12), a `floor` (the join threshold, D-IC-7) and a `min_issue_size` (D-IC-8). `score_names` and `feedback_sources` are JSON arrays of strings, NULL when empty. `paused_detail` holds the provider's error text for a connection pause. `next_refit_at` is when the next refit is due, nanoseconds (D-IC-8). `refit_trigger` is the trigger that the due refit records, `schedule` or `manual` (CHECK): "Refit now" sets it to `manual` with `next_refit_at`, the refit records it in `issue_refits.trigger`, and the refit's finish schedules the next one and sets it back to `schedule`.

`issues` holds one error group per row. `fingerprint` is the hex fingerprint of the group (D-IC-17); it is unique per source where set. `state` is `open`, `resolved`, `ignored` or `merged`; `regressed` is set when a resolved issue recurs. `merged_into` names the issue this one was merged into; a merge chain is one level deep (D-IC-9). `first_seen` and `last_seen` are trace start times, not ingest times. `resolved_at` and `resolved_by` record the last resolution. For an LLM source's issue, `centroid` is the mean of the member vectors and `member_count` the members in it (at most 500, D-IC-7); both are NULL or zero for error issues.

`issue_events` is the audit trail of an issue (D-IC-10): `kind` is `created`, `state_changed`, `renamed`, `merged` or `regressed`; `from_value` and `to_value` hold the old and new value as text. `actor_user_id` NULL means the system. Events go with their issue.

`issue_candidates` is the queue of an LLM source's work (D-IC-4, D-IC-12): one row per accepted target, deduplicated by `dedup_key`. `dedup_key` is `<source id>:<trace hex>` for a trace target, `<source id>:<trace hex>:<span hex>` for a span and `<source id>:session:<session id>` for a session. `status` is `queued`, `running`, `pending` (waiting for a refit), `assigned`, `outlier`, `dropped`, `failed` or `skipped`. `attempts`, `claims`, `interruptions` and `load_retries` count the queue's tries; `not_before` is when the row may be claimed (ns). `summary` and `summary_prompt_version` hold the one summary; `vector` and `embed_model`/`dimension` hold the embedding, a f32 little-endian BLOB. `call_trace_id` names the call trace of the last LLM call, and `cost_usd` sums the spend of its calls. Candidates keep their vector 7 days unassigned and 30 days assigned (D-IC-6). A finished candidate's (`assigned`, `outlier`, `dropped`, `failed` or `skipped`) `summary` is cleared when it is older than its project's `retention_days` (the row stays, since it anchors the issue membership); a `queued`, `running` or `pending` candidate keeps its summary, since a refit's naming prompt needs it; the candidates whose trace starts before the org's data window are deleted. A candidate that duplicates a `dedup_key` takes no slot of the source's daily cap.

`issue_source_days` counts, per source and UTC day (`day` is the day start, ns), the candidates admitted and the USD spent: the daily cap and the budget read it, as `rule_spend` does for online rules (D-IC-1).

`issue_refits` is the history of a source's refits (D-IC-8, D-IC-12): `trigger` is `schedule` or `manual`; the counts are the pending candidates read, the clusters found, the issues created, the candidates joined and the outliers; `error_class` is set when the refit stopped early. The last 50 refits of a source are kept.

`issue_sources.last_notified_at` is the cut-off of the source's notifications, in nanoseconds: enabling a source, or giving it its first channel, sets it to now, so no backlog is sent (D-IC-21). `issue_source_channels` holds the source's notification channels, for new and regressed issues: at most 10 per source, both foreign keys cascade. `issue_notifications` holds one digest per message: `digest` is the JSON `{"items":[{"issue_id","kind":"created"|"regressed","title"}],"more":n}`, at most 20 items, newest first, `more` the count of the rest. A source sends at most one digest per 10 minutes, to each of its channels, through the delivery loop of § Alerts. Digests are pruned like alert events, after 90 days, with their deliveries (D-IC-21).

```sql
CREATE TABLE issue_sources (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,                 -- 'error', 'score', 'feedback' or 'topic'; no CHECK (D-IC-1)
  filter TEXT,                        -- span filter DSL (error) or trace filter DSL (LLM kinds), when set
  enabled INTEGER NOT NULL DEFAULT 1,
  paused_reason TEXT,                 -- set by the sweeper or a connection pause; NULL when not paused
  paused_at INTEGER,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  sample_rate REAL,                   -- LLM kinds, (0, 1]
  daily_cap INTEGER,                  -- LLM kinds, candidates per UTC day
  score_names TEXT,                   -- JSON array of strings; NULL when empty
  feedback_sources TEXT,              -- JSON array of strings; NULL when empty
  chat_connection_id TEXT REFERENCES llm_connections(id),
  chat_model TEXT,
  embed_connection_id TEXT REFERENCES llm_connections(id),
  embed_model TEXT,
  embed_dimension INTEGER,            -- set once, at create or on the first embedding
  floor REAL,                         -- join threshold, cosine
  min_issue_size INTEGER,
  budget_usd REAL,
  budget_period TEXT,                 -- 'day' or 'month'
  paused_detail TEXT,                 -- the provider's error text for a connection pause
  next_refit_at INTEGER,              -- ns
  refit_trigger TEXT NOT NULL DEFAULT 'schedule' CHECK (refit_trigger IN ('schedule', 'manual')),
  last_notified_at INTEGER,           -- ns; the last issue notification cut-off (D-IC-21)
  UNIQUE (project_id, name)
);

CREATE TABLE issues (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  source_id TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
  fingerprint TEXT,                   -- NULL for issues without one
  title TEXT NOT NULL,
  description TEXT,
  state TEXT NOT NULL CHECK (state IN ('open', 'resolved', 'ignored', 'merged')),
  regressed INTEGER NOT NULL DEFAULT 0,
  merged_into TEXT REFERENCES issues(id) ON DELETE SET NULL,
  first_seen INTEGER NOT NULL,        -- trace start, ns
  last_seen INTEGER NOT NULL,         -- trace start, ns
  resolved_at INTEGER,
  resolved_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  centroid BLOB,                      -- f32 little-endian; NULL for error issues
  member_count INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX issues_source_fingerprint ON issues (source_id, fingerprint)
    WHERE fingerprint IS NOT NULL;
CREATE INDEX issues_project_last_seen ON issues (project_id, last_seen DESC, id);
CREATE INDEX issues_merged_into ON issues (merged_into) WHERE merged_into IS NOT NULL;
CREATE INDEX issues_source ON issues (source_id);

CREATE TABLE issue_events (
  id TEXT PRIMARY KEY,
  issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('created', 'state_changed', 'renamed', 'merged', 'regressed')),
  actor_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
  from_value TEXT,
  to_value TEXT,
  at INTEGER NOT NULL                 -- ns
);
CREATE INDEX issue_events_issue ON issue_events (issue_id, at, id);

CREATE TABLE issue_candidates (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  source_id TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
  target TEXT NOT NULL CHECK (target IN ('trace', 'span', 'session')),
  trace_id BLOB,
  span_id BLOB,
  session_id TEXT,
  user_id TEXT,
  trace_start INTEGER,                -- ns
  score_id TEXT,                      -- the score that made the candidate, when a score source
  dedup_key TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'pending', 'assigned', 'outlier', 'dropped', 'failed', 'skipped')),
  attempts INTEGER NOT NULL DEFAULT 0,
  claims INTEGER NOT NULL DEFAULT 0,
  interruptions INTEGER NOT NULL DEFAULT 0,
  load_retries INTEGER NOT NULL DEFAULT 0,
  not_before INTEGER NOT NULL,        -- ns
  error_class TEXT,
  issue_id TEXT REFERENCES issues(id) ON DELETE SET NULL,
  summary TEXT,
  summary_prompt_version INTEGER,
  vector BLOB,                        -- f32 little-endian
  embed_model TEXT,
  dimension INTEGER,
  call_trace_id BLOB,
  cost_usd REAL NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE INDEX issue_candidates_ready ON issue_candidates (not_before) WHERE status = 'queued';
CREATE INDEX issue_candidates_source ON issue_candidates (source_id, status, created_at);
CREATE INDEX issue_candidates_trace ON issue_candidates (project_id, trace_id);

CREATE TABLE issue_source_days (
  source_id TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
  day INTEGER NOT NULL,               -- UTC day start, ns
  candidates INTEGER NOT NULL DEFAULT 0,
  spent_usd REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (source_id, day)
);

CREATE TABLE issue_refits (
  id TEXT PRIMARY KEY,
  source_id TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
  trigger TEXT NOT NULL CHECK (trigger IN ('schedule', 'manual')),
  started_at INTEGER NOT NULL,        -- ns
  finished_at INTEGER,                -- ns; NULL while running
  pending INTEGER NOT NULL DEFAULT 0,
  clusters INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL DEFAULT 0,
  joined INTEGER NOT NULL DEFAULT 0,
  outliers INTEGER NOT NULL DEFAULT 0,
  cost_usd REAL NOT NULL DEFAULT 0,
  error_class TEXT
);
CREATE INDEX issue_refits_source ON issue_refits (source_id, started_at DESC);

CREATE TABLE issue_source_channels (
    source_id  TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    channel_id TEXT NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    PRIMARY KEY (source_id, channel_id)
);
CREATE INDEX issue_source_channels_channel ON issue_source_channels (channel_id);

CREATE TABLE issue_notifications (
    id         TEXT PRIMARY KEY,
    source_id  TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    digest     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX issue_notifications_created ON issue_notifications (created_at);
```

## Alerts

Track K (07, D-K-1–D-K-16). `config_enc` is sealed JSON (§ Encryption of secrets) and is listed in `crypto::ENCRYPTED_COLUMNS`. `alert_rules.config` is the normalized condition JSON (07); `state` is `ok`, `firing` or `no_data`; `clear_streak` counts clear or insufficient evaluations while firing. `alert_events.kind` is `fired`, `resolved` or `reminder`, and `details` snapshots the rule's name, metric, comparator, window and filter. `notification_deliveries.status` is `pending`, `sent` or `failed`; `next_attempt_at` is set while pending. Events are kept 90 days, finished deliveries 30 days (D-K-10). A delivery belongs to an alert event or to an issue notification, never both (D-IC-21).

```sql
CREATE TABLE notification_channels (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  config_enc BLOB NOT NULL,
  target_hint TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX notification_channels_name ON notification_channels(project_id, name);

CREATE TABLE alert_rules (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  metric TEXT NOT NULL,
  config TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  cooldown_s INTEGER NOT NULL DEFAULT 3600,
  notify_while_firing INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL DEFAULT 'ok',
  clear_streak INTEGER NOT NULL DEFAULT 0,
  last_value REAL,
  last_count INTEGER,
  last_evaluated_at INTEGER,
  last_fired_at INTEGER,
  last_notified_at INTEGER,
  last_error TEXT,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX alert_rules_name ON alert_rules(project_id, name);
CREATE INDEX alert_rules_due ON alert_rules(enabled, last_evaluated_at);

CREATE TABLE alert_rule_channels (
  rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
  channel_id TEXT NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
  PRIMARY KEY (rule_id, channel_id)
);
CREATE INDEX alert_rule_channels_channel ON alert_rule_channels(channel_id);

CREATE TABLE alert_events (
  id TEXT PRIMARY KEY,
  rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  value REAL,
  count INTEGER,
  threshold REAL NOT NULL,
  window_from INTEGER NOT NULL,
  window_to INTEGER NOT NULL,
  details TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX alert_events_rule ON alert_events(rule_id, created_at);
CREATE INDEX alert_events_created ON alert_events(created_at);

CREATE TABLE notification_deliveries (
    id                    TEXT PRIMARY KEY,
    event_id              TEXT REFERENCES alert_events(id) ON DELETE CASCADE,
    issue_notification_id TEXT REFERENCES issue_notifications(id) ON DELETE CASCADE,
    channel_id            TEXT NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    status                TEXT NOT NULL,
    attempts              INTEGER NOT NULL DEFAULT 0,
    next_attempt_at       INTEGER,
    last_error            TEXT,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    CHECK ((event_id IS NULL) <> (issue_notification_id IS NULL))
);
CREATE INDEX notification_deliveries_due ON notification_deliveries(status, next_attempt_at);
CREATE INDEX notification_deliveries_event ON notification_deliveries(event_id);
CREATE INDEX notification_deliveries_channel ON notification_deliveries(channel_id, updated_at);
CREATE INDEX notification_deliveries_issue ON notification_deliveries(issue_notification_id);
```

## Entitlements (track P)

```sql
CREATE TABLE org_entitlements (           -- the newest accepted org token (D-P-11)
  org_id TEXT PRIMARY KEY REFERENCES orgs(id) ON DELETE CASCADE,
  token TEXT NOT NULL,                    -- as received; never logged
  token_id TEXT NOT NULL,
  issued_at INTEGER NOT NULL,
  received_at INTEGER NOT NULL
);
CREATE TABLE org_window_ends (            -- data windows that stopped applying, kept 30 days (D-P-30)
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  ended_at INTEGER NOT NULL,
  data_access_days INTEGER,               -- NULL = unlimited
  PRIMARY KEY (org_id, ended_at)
);
CREATE TABLE usage_notices (              -- one row per org, month and threshold: each notice is sent once (D-P-18, D-P-38)
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  month TEXT NOT NULL,                    -- YYYY-MM, UTC
  threshold INTEGER NOT NULL,             -- 80 or 100
  sent_at INTEGER NOT NULL,
  PRIMARY KEY (org_id, month, threshold)
);
```

`orgs` has no `limits` column: track P's M2 drops it (D179, D-P-9).

## Metering & instance

```sql
CREATE TABLE usage_daily (
  org_id TEXT NOT NULL, project_id TEXT NOT NULL, day TEXT NOT NULL,
  spans INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0,
  scores INTEGER NOT NULL DEFAULT 0,
  judge_input_tokens INTEGER NOT NULL DEFAULT 0, judge_output_tokens INTEGER NOT NULL DEFAULT 0,
  judge_cost_usd REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (org_id, project_id, day)
);

CREATE TABLE instance_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- 'encryption.check': hex HMAC-SHA256 check value of the encryption key (D-I-2)
-- 'license.key': the licence key an instance admin installed, plain text (D-P-75)
```

`instance_settings` is created by track I (migration `0004`, D156).

Usage counters are accumulated in memory and flushed to `usage_daily` every 10 s (upsert-add). A dropped org's rows stay 40 days after its org deletion is applied, then a daily task deletes them (D-P-46).

`spans` counts billable spans: spans in the reserved `cetra-eval` environment (judge traces) are stored but not counted (D-I-31). Judge calls add their tokens and cost to `judge_input_tokens`, `judge_output_tokens` and `judge_cost_usd`.

## Per-org SSO (track P, `ee`)

One OIDC provider and any number of email domains per org (D-P-52, D-P-54). The provider's client secret is sealed like an LLM connection key (§ Encryption of secrets); `org_sso_providers.client_secret_enc` is listed in `crypto::ENCRYPTED_COLUMNS`. The sealed value's associated data is the row's `id`, so a sealed secret is bound to its row. A domain is verified by at most one org at a time (the partial unique index). `jit_role` is the role of people who join through the provider (D-P-56). `last_login_at` is set by a successful sign-in through the provider and gates enforcement (D-P-57); changing the issuer or client id resets it, so a new Test sign-in is needed.

```sql
CREATE TABLE org_sso_providers (
    id TEXT PRIMARY KEY,                -- UUIDv7; the row id in client_secret_enc's associated data
    org_id TEXT NOT NULL UNIQUE REFERENCES orgs(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    issuer TEXT NOT NULL,
    client_id TEXT NOT NULL,
    client_secret_enc BLOB NOT NULL,    -- sealed (D-I-2)
    enabled INTEGER NOT NULL DEFAULT 0,
    enforced INTEGER NOT NULL DEFAULT 0,
    jit_role TEXT NOT NULL DEFAULT 'viewer',   -- 'viewer' | 'member'
    last_login_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE org_sso_domains (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    domain TEXT NOT NULL,               -- lowercase, no trailing dot
    token TEXT NOT NULL,                -- 32 lowercase hex; the TXT value's random part (public)
    verified_at INTEGER,
    last_checked_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE (org_id, domain)
);

CREATE UNIQUE INDEX org_sso_domains_verified ON org_sso_domains(domain) WHERE verified_at IS NOT NULL;
```

Deleting an org deletes its provider and domains by cascade, and its identities (`identities.provider = 'org:<org_id>'`) explicitly, since no key references the org from `identities`.

## Audit records (track EA, `audit_events`)

Every access, membership and configuration action writes one row. The row is written in the transaction of the change it records, or, for an action that changes nothing in `meta.db`, in a transaction of its own before the action runs. A failed write fails the request (D-EA-2). Rows carry ids only; names and emails are joined at read time (D-EA-3). The catalog and the read API are in `docs/plans/ea-enterprise-admin/README.md` § Audit log.

```sql
CREATE TABLE audit_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    at INTEGER NOT NULL,
    org_id TEXT,
    project_id TEXT,
    action TEXT NOT NULL,
    outcome TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    actor_id TEXT,
    via TEXT,
    credential_id TEXT,
    target_kind TEXT,
    target_id TEXT,
    details TEXT NOT NULL DEFAULT '{}',
    ip TEXT,
    user_agent TEXT
);
CREATE INDEX audit_events_org ON audit_events (org_id, seq);
CREATE INDEX audit_events_org_action ON audit_events (org_id, action, seq);
CREATE INDEX audit_events_org_actor ON audit_events (org_id, actor_id, seq);
CREATE INDEX audit_events_org_target ON audit_events (org_id, target_id, seq);
CREATE INDEX audit_events_actor ON audit_events (actor_id);
CREATE INDEX audit_events_at ON audit_events (at);
```

| Column | Meaning |
|---|---|
| `seq` | The sort, paging and stream cursor (D-EA-20). `meta.db` has one writer, so `seq` follows commit order. |
| `id` | UUIDv7, the public id. Not a cursor. |
| `at` | Write time, `i64` nanoseconds, UTC. |
| `org_id` | The org; null for an instance record (D-EA-8). |
| `project_id` | The project, for a project-scoped action. |
| `action` | A dotted, append-only name from the catalog, e.g. `member.role_changed`. |
| `outcome` | `success`, or `failure` for the actions the catalog marks as recording failures. |
| `actor_kind`, `actor_id` | `user`, `api_key`, `scim_token`, `control_plane`, `cli`, `system` or `anonymous`, with the id of the user, key or control-plane token; null when there is none. |
| `via`, `credential_id` | For a user: `session`, `pat` or `device`; `credential_id` is the PAT id. Never a token value. |
| `target_kind`, `target_id` | The resource the action acted on, by kind and id. |
| `details` | JSON with the fields the action's catalog entry allows: names of changed fields, roles, scopes, flags, limits. Never secrets, emails, names, payloads or URLs. |
| `ip`, `user_agent` | The client IP (spec 08 § Client IP) and the user agent, truncated to 256 characters. |

- **No foreign keys.** Records outlive the users, orgs and projects they name. The table has no `CHECK`: action names are append-only, and a `CHECK` would make each new name a format change (D-R-34).
- **Purge.** An hourly task deletes records whose `at` is older than `audit.retention_days` (spec 12), in batches of 5,000 rows, each batch its own write. `0` keeps everything. The task is off in restore mode.
- **Org drop.** When an org's drop is applied, its rows are deleted in the drop's transaction, and an instance record `org.dropped` is written.
- **User deletion.** In the user's deletion transaction, the `ip` and `user_agent` of the rows whose actor is that user are cleared. The rows keep their ids.

## Encryption of secrets

The key is 32 bytes in standard base64. Source order (D-I-1): the `encryption_key` config value (`CETRA_ENCRYPTION_KEY`) wins and the file is neither read nor written; otherwise `<data_dir>/secret.key`; otherwise the server generates a key, but only while `instance_settings` has no `encryption.check` — once a check value exists, a missing key fails startup. The file holds the base64 of the 32 key bytes plus one trailing newline, mode 0600, written through a `.tmp` file and a rename so a crash leaves either no file or a whole one; a mode readable by group or other is warned about and still used. `cetra admin rotate-encryption-key` re-encrypts every sealed column in one transaction, writing `secret.key.new` first; a start that finds a `secret.key.new` whose check value matches the stored one renames it over `secret.key`, finishing an interrupted rotation, and otherwise deletes it (D-I-3).

A sealed value is `0x01` ‖ key id (4 bytes) ‖ nonce (12) ‖ AES-256-GCM ciphertext and tag (D-I-2). The key id is `HMAC-SHA256(key, "cetra.key-id.v1")[0..4]`; the associated data is `table:column:row_id`, with the row id a hyphenated lowercase UUID; the nonce is 96 random bits. `instance_settings['encryption.check']` holds the lowercase hex of `HMAC-SHA256(key, "cetra.encryption-check.v1")`: the server records it on the first start of a data directory and refuses a different key afterwards.

## Format versions (D-R-33 to D-R-38)

From the first public release (D185), a `meta.db`, `secret.key` and backup written by any release must open with every later one, and an older binary must refuse a newer one without changing it. Before that release there is no constraint. The storage side (`catalog.db`, WAL, Parquet) is 03 § 12. Before the first public release `META_FORMAT_VERSION` stays 1: a format change then needs no bump (D185).

**Two numbers.** The applied migrations (`_sqlx_migrations`) are the schema. `PRAGMA user_version` holds `META_FORMAT_VERSION` (`crates/server/src/meta/mod.rs`, a `u16`, 1 at the first release): the format of what `meta.db` means, and of `secret.key` and the sealed envelope (§ Encryption of secrets). It is the high-water mark: a binary writes it before it first writes anything in a newer format, and never lowers it. `VACUUM INTO` keeps it, so pre-migration copies and backups carry it.

**Opening.** One check runs before any write, in this order: a `user_version` other than 0 or this build's is refused (`meta.db has format version N, which this build … does not read`); an applied migration this build does not embed is refused (`meta.db has applied migration N, which this build does not have`); `user_version` 0 with a row in `users` or `orgs` is a pre-release database and is refused (`PreFormat`); `user_version` 0 without rows is stamped after the migrations. `cetra serve`, `cetra admin migrate` and `cetra admin gc-orphans` run this check on `<data_dir>/meta.db` **before storage opens** (the preflight), so a newer `meta.db` stops them before `LOCK`, the WAL or the object store is touched; the other admin commands run it when they open `meta.db`. `reapply-deletions --from` accepts a newer schema of the same format (D-M-9) but not a newer format. Downgrading is not supported: restore the backup taken before the upgrade.

**What is a format change.** Not one: a migration that only adds (`CREATE TABLE`, `CREATE INDEX`, `CREATE UNIQUE INDEX`, `ALTER TABLE … ADD COLUMN` that is nullable or has a default). A format change, which bumps `META_FORMAT_VERSION`: any other migration (a dropped or renamed column or table, an `UPDATE` of existing rows, a table rebuild); a new value in a TEXT enum column (roles, kinds, states, statuses); a changed JSON shape in a JSON column; a changed sealed envelope, key id label, check label or associated data; a changed `secret.key` format. A lint requires migrations to be additive unless listed as format-changing (the pre-release exceptions are `0008`, which drops `orgs.limits`, and `0030`, which sets `issue_sources.last_notified_at` and rebuilds `notification_deliveries`); every migration that alters a table has a test that its rows survive; the schema snapshot (`crates/server/tests/snapshots/meta_schema.snap`) shows every schema change. A migration that has been in a release is never edited.

**`secret.key` and sealed values** keep the formats of § Encryption of secrets: no version byte in the file. A key file this build does not decode is refused and never rewritten; a new file format would bump `META_FORMAT_VERSION`, which is checked first. Key rotation is the offline `cetra admin rotate-encryption-key` (D-I-3). Known-answer tests pin the file text, the key id, the check value and one sealed value for a fixed public test key.

**A release that changes the format** bumps `META_FORMAT_VERSION`, migrates at open (the migration or the open code rewrites what the older release wrote), has tests that restore the previous releases' golden backups, and adds a line under `### Upgrade notes` in `CHANGELOG.md`.

**Golden backups.** `fixtures/server/golden/<version>/` is an online backup taken by the release's own build of a scenario server with a row in every table (`just golden-dir <version>`), with the `admin.sock` lines that produced it and `expected.json`. A test restores a copy of each one with the current build by the procedure of 12 § Backups, compares the rows by column name, the decoded reads, the opened secrets and the sign-ins, serves it, writes to it and rotates its key. None is committed before the first stable release; until then the test builds one into a temp dir and checks it. `fixtures/storage/golden/RELEASES` lists the released versions for both (03 § 12).
