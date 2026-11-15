Status: T1–T7 done on 2026-11-15

# T · Model prices

Track T adds what the MVP scope promises and nobody built: per-org and per-project custom model prices (`model_prices`, spec 04 § Pricing), the `/models` API (spec 09 § Pricing & GDPR), a server price registry that prices every span and judge call with the project's table instead of the built-in snapshot alone, and the two settings pages (spec 11). The coordinator created the track by D184 (cross-track review X29). G left the hook for it: the `RwLock<Arc<PriceTable>>` in `crates/server/src/ingest/mod.rs` and its `#[cfg(test)] replace_prices` (D-G-10). T removes both.

The main use: a local Ollama or vLLM judge has no built-in price, so D-I-15 refuses a budgeted online rule on it. With T, a project admin adds a price for the model (even `0`), and the rule can carry a budget.

The plan has 7 sub-phases, **T1–T7**, one file each. Each sub-phase is one commit, done in order by one agent working alone. The implementer is Haiku; nothing in a phase file may need a guess. If a phase file and the code disagree (a name moved, a function changed), STOP and report instead of guessing. A sub-phase ends with `just check` green.

Model prices are core (MIT), not `ee/` code: every plan computes cost.

## Prerequisite

Tracks B1, G and I (M1–M6) are merged on `main`. T1 checks:

```bash
grep -n "pub fn price_for" crates/otel/src/cost.rs
```

```bash
grep -n "pub(crate) fn replace_prices" crates/server/src/ingest/mod.rs
```

Each must print one line. Otherwise STOP and report.

## Read first, for every sub-phase

- `AGENTS.md`;
- this README, all of it;
- the sub-phase file;
- `docs/spec/02-ingestion.md` § Cost computation, `docs/spec/04-metadata-db.md` § Pricing, `docs/spec/09-api.md` § Pricing & GDPR (all three as amended by this plan);
- the existing files the sub-phase names. Read every file before you edit it.

## Files

| File | Sub-phase |
|---|---|
| [01-layered-table.md](01-layered-table.md) | T1 `PriceTable::layered` in `llmobs-otel`: custom rows over a shared built-in table |
| [02-store.md](02-store.md) | T2 migration and the `meta::model_prices` store |
| [03-registry.md](03-registry.md) | T3 `crate::prices::Prices`: load, per-project tables, reload; every caller prices with it; `replace_prices` removed |
| [04-project-api.md](04-project-api.md) | T4 API types, validation, the rule guard, project routes (`/models`, `/models/resolve`), OpenAPI, fixtures |
| [05-org-api.md](05-org-api.md) | T5 `OrgPerm::ManagePrices` and the org routes, OpenAPI, fixtures |
| [06-web-dialog-org.md](06-web-dialog-org.md) | T6 API helpers, the price dialog, the org Model prices page, write mocks |
| [07-web-project.md](07-web-project.md) | T7 the project Model prices page, the online-rule hint, closeout |

## Design

### Model

```
built-in snapshot (crates/otel/prices/builtin.json, embedded, D104)
        │  compiled once at startup: Arc<PriceTable>  (≈1 900 rows)
        ▼
model_prices (meta.db)          org rows: project_id IS NULL
  org_id, project_id?, …        project rows: project_id set
        │  loaded at startup, reloaded per org after each write
        ▼
Prices registry (crates/server/src/prices.rs)
  builtin: Arc<PriceTable>
  per org with rows:      PriceTable::layered(builtin, org rows)
  per project with rows:  PriceTable::layered(builtin, org rows ++ project rows)
        │
        ▼
for_project(org, project) → Arc<PriceTable>   (project table, else org table, else builtin)
for_org(org)              → Arc<PriceTable>   (org table, else builtin)
```

A project's effective table is the built-in snapshot, its org's rows and its own rows, and nothing else (D-T-3). The lookup is spec 02's rule 2 unchanged: candidates from every layer, then scope (project > org > built-in), then the latest `valid_from`, then the longest pattern, then the lowest index. `PriceTable::layered` (T1) gives exactly that without copying or recompiling the built-in rows for each project.

### Who prices with what (T3)

| Caller | Today | After T3 |
|---|---|---|
| OTLP ingest (`Ingest::accept_request`, HTTP and gRPC) | `self.prices` | `self.prices.for_project(project.org_id, project.id)` |
| Judge runs (`eval::execute::run_evaluator`, estimate in `reserve_budget`) | `state.ingest.prices()` | the job's project table |
| Rule checks (`routes::online_rules::validate`), evaluator PATCH (`routes::evaluators`) | `state.ingest.prices()` | the route's project table |
| Backfill estimate (`routes::backfills`) | `state.ingest.prices()` | the route's project table |
| Connection test (`routes::connections`) | `state.ingest.prices()` | `for_org(org)`: a connection has no project |

### Writes (T4, T5)

```
POST/PATCH/DELETE (project or org row)
  lock prices.writes (tokio Mutex, one price write at a time per process)
  meta.write(tx):
     read the org's rows (org + every project of the org)
     apply the change in memory → candidate rows
     validate: compile the candidate table of each affected scope (PriceTable::layered)
     guard (D-T-8): every budgeted judge rule of the affected projects still has a price
     write the row (insert / update / delete), unique index → 409
  prices.reload_org(reader, org)   (read the org's rows again, rebuild its tables, swap)
  unlock
```

Writes are rare, so one mutex for all of them keeps reloads in order: a reload always installs the state of the last committed write.

### Modules (end state)

```
crates/otel/src/cost.rs                       PriceTable::layered, rows(), base() (T1)
crates/server/migrations/meta/NNNN_model_prices.sql                                (T2)
crates/server/src/meta/model_prices.rs        store: rows, conversions, budgeted judges (T2)
crates/server/src/prices.rs                   Prices registry (T3); validate_input, candidate tables, guard (T4)
crates/server/src/api/model_prices.rs         ModelPrice, ModelPriceInput, ModelPriceScope, … (T4)
crates/server/src/routes/model_prices.rs      project routes (T4), org routes (T5)
web/src/api/queries-prices.ts                 query helpers (T6)
web/src/components/prices/*                   price dialog, tiers editor, price table (T6, T7)
web/src/routes/$org/settings/models.tsx                                             (T6)
web/src/routes/$org/$project/settings/models.tsx                                    (T7)
```

### Migration number

Other tracks add migrations in parallel (`main` ends at `0015_email_verified_via.sql` today). T's migration takes **the next free number when T2 is implemented**: the highest `NNNN` in `crates/server/migrations/meta/` plus one, zero-padded to four digits (`0016_model_prices.sql` if `0015_email_verified_via.sql` is still the last). If another branch lands the same number before T merges, renumber T's file at merge and regenerate the schema snapshot (the tests read `expected_version()`, so no version literal changes; T2 § Schema version).

## Decisions

These are written in `docs/mvp-decisions.md` § Track T and in specs 02, 04, 09 and 11, in the same commit as this plan.

| # | Decision |
|---|---|
| D-T-1 | **A plan, no separate design doc.** One milestone, seven sub-phases T1–T7, the short design in this README. |
| D-T-2 | **Built-in rows are not stored in `meta.db`.** The snapshot stays embedded and is compiled once at startup. `model_prices` holds org and project rows only, and the `scope` column goes: a row with `project_id` NULL is an org row, else a project row. Spec 04's "re-seed the built-in rows at each startup" is dropped: copying about 1 900 rows into SQLite at every start buys nothing, since the API serves them from memory. |
| D-T-3 | **Override rules: spec 02 rule 2, unchanged.** A project's table is the built-in snapshot, its org's rows and its own rows. Candidates are every matching row of the three layers; the winner is the highest scope (project > org > built-in), then the latest `valid_from`, then the longest pattern, then the lowest index. So a custom row overrides the built-in rows for every model name its pattern matches, whatever their `valid_from`. A custom row with a provider prices only spans of that provider (or with none, rule 2); a custom row without a provider prices any provider's span. Another org's or project's rows never apply. |
| D-T-4 | **A row.** `model_name` 1–200 characters after trimming. `match_pattern` is optional on write: absent, it is `regex::escape(model_name)`, so the row matches exactly that name (case-insensitive, anchored). A given pattern is 1–500 bytes and must compile alone and in the scope's table. `provider` is absent or 1–64 characters of `[a-z0-9._-]`, starting with a letter or digit (the semconv names spans carry, e.g. `openai`, `gcp.vertex_ai`). `valid_from` defaults to 0. Prices are USD per 1M tokens, each absent or finite in `[0, 1 000 000]`; `price_input` or `price_output` must be present. Up to 8 tiers, `min_input_tokens` ≥ 1 and strictly increasing, each tier with at least one price. At most 500 org rows per org and 500 rows per project. The same `(match_pattern, provider, valid_from)` twice in one scope is `409`. |
| D-T-5 | **Not retroactive.** A price write applies to spans accepted after it (and, through `valid_from`, only to spans that started at or after `valid_from`). Stored costs are never recomputed; a recompute job stays post-MVP (spec 02). Judge spend already recorded in `rule_spend` and `usage_daily` stays. The pages say so. |
| D-T-6 | **Permissions.** Project rows: `ProjectPerm::ManageSettings` (project admin; API keys cannot). Org rows: a new `OrgPerm::ManagePrices` (org admin; not session-only, so a PAT of an admin may script it). Reads: `GET /projects/{project}/models` and `…/models/resolve` need `ProjectPerm::View` (a `read` key may list them); `GET /orgs/{org}/models` needs `OrgPerm::View`. Every write is an audit event (`target: "audit"`) with the row id, org id, project id and user id, never the model name or pattern. |
| D-T-7 | **API shape.** `GET /api/v1/projects/{project}/models?scope=&q=&limit=&cursor=` answers the effective table, paged: project rows, then org rows, then built-in rows; inside a scope by lowercased `model_name`, then id (built-in rows have no id; their sort id is `builtin:` and their 6-digit snapshot index). `q` is a case-insensitive substring of `model_name`. `GET /api/v1/projects/{project}/models/resolve?model=&provider=` answers the row that would price that model now (`{price}`, with `price` absent when none does). `POST /api/v1/projects/{project}/models` creates a project row (`201`); `PATCH` and `DELETE /api/v1/projects/{project}/models/{id}` change or delete one. `GET/POST /api/v1/orgs/{org}/models` and `PATCH/DELETE /api/v1/orgs/{org}/models/{id}` do the same for org rows; the org list holds the org's rows only and is not paged (≤ 500). A `PATCH` body is the whole row (`ModelPriceInput`, as `POST`): it replaces every field, and an absent optional field becomes null, so the dialog sends what it shows. Built-in rows cannot be changed; a custom row overrides them. |
| D-T-8 | **A price write cannot break a budgeted rule.** As D-I-19 does for an evaluator `PATCH`: a `POST`, `PATCH` or `DELETE` (project or org) after which a budgeted judge rule in an affected project that had a price for its model would have none (`eval::budget::model_price` is `None`) is `409` (`conflict`) with one error per rule, field `rules.<rule id>`, message `<rule name>: the judge model would have no price`. The check runs in the write transaction against the candidate table. Disabled rules count too, so enabling one later cannot fail on price. |
| D-T-9 | **The registry.** The built-in table is compiled once at startup. Org and project rows are loaded at startup and compiled into layered tables per org and per project that have rows; the ingest path takes a read lock and an `Arc` clone, and never touches `meta.db`. Each price write reloads its org from `meta.db` under one process-wide write mutex. A purged org or project has its tables dropped, under the write mutex. A reload that fails after a committed write is logged and leaves the org's old tables until its next write or a restart; accepted. A stored row that no longer compiles at startup (a newer regex rule, a hand edit) does not stop the server: that org's tables fall back to the built-in table and an error with the org id is logged. |
| D-T-10 | **"LiteLLM import" is the built-in snapshot.** The MVP line "LiteLLM import + per-project custom prices" is met by B1's committed snapshot (`just prices-update`, D104) plus T's custom rows. There is no upload of a LiteLLM JSON file or a bulk import endpoint in the MVP; rows are added one at a time in the UI or the API. |
| D-T-11 | **Local models and budgets (D-I-15).** A judge on a local Ollama or vLLM server goes through the OpenAI-compatible adapter, so `model_price` asks with provider `openai`. A project or org row without a provider (or with `openai`) whose pattern matches the model prices it. A price of `0` is allowed: the rule can then carry a budget that never pauses it. The budget error says where to add a price, and the online-rule editor links to the project's Model prices page. |
| D-T-12 | **UI.** Org settings get "Model prices": the org's rows; admins add, edit and delete. Project settings get "Model prices": the project's own rows (admins edit), the org's rows (read-only, with a link to the org page), the built-in rows (search), and a "Which price applies?" box that calls `/models/resolve`. One dialog serves both scopes: exact name or regex, provider, valid from (optional date), five prices, up to 8 tiers. Both pages say that new prices apply to spans received from now on and that stored costs are not recomputed. |

## Contract gaps

- **CG-T1.** `/models/resolve` answers at the current time only; it has no `at` parameter. A user checking a row with a future `valid_from` sees today's winner. Accepted.
- **CG-T2.** There is no project-wide list of the models seen in traces with no price ("unpriced models"). A later track can add it from storage (`cost_source = 'none'` on `llm` spans grouped by model).

## Conventions

- **Time**: `i64` nanoseconds in Rust and `meta.db`; RFC 3339 strings with 9 fractional digits in JSON (`llmobs_core::time_serde`, as the existing API types).
- **IDs**: custom rows are UUIDv7 (`Uuid::now_v7()`), stored as `TEXT`.
- **Errors**: problem+json through `crate::http::error::ApiError`. Field errors name the JSON field path, e.g. `tiers[2].min_input_tokens`.
- **Privacy**: logs carry row, org, project and user ids, counts and error classes. Never a model name, a pattern or a provider: they are user input (AGENTS.md § Privacy). `PriceError` already carries only an index.
- **No `unwrap`/`expect`** outside tests unless a comment states the invariant.
- **Tests**: Rust tests sit in the module they test (`#[cfg(test)] mod tests`). Web tests use MSW with `server.use` overrides and never `vi.mock`.

## Commands

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
just check
```

`just check` must pass before every commit. When a sub-phase changes the API contract, run `just openapi` first and commit `crates/server/openapi.json` and `web/src/api/schema.gen.ts`; when it changes `fixtures/api/`, run `just api-examples` and commit `fixtures/api/manifest.json`. Focused runs while working: `cargo test -p llmobs-otel cost`, `cargo test -p llmobs-server prices`, `cargo test -p llmobs-server model_prices`, `pnpm --dir web test -- models`.

## Report

When a sub-phase is done, report what was done, any deviation and why, versions of any new dependency (T adds none), and anything left undone or unverified.
