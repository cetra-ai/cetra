# Sessions list: what each session is about, readable numbers, summary

Plan for Haiku implementers, one phase at a time. Read the whole of your phase before editing.

## Conventions (every phase)

- **Never guess.** If something in your phase is unclear, contradicts the code, or a step fails in a way the phase does not explain, STOP. Make no further edits. Your final message must start with `QUESTION:` followed by the question and what you found. You will be resumed with an answer.
- Work directly in `/home/sebastien/travaux/llm-observer` on `main`. Do not create branches or worktrees. Do not commit, stash, reset or switch branches.
- Edit only the files your phase lists. Other phases run at the same time on other files.
- Before any node command: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
- Web checks, from `web/`: `./node_modules/.bin/vitest run <paths>`, `npx tsc -b` (no errors). Biome from the repo root: `node_modules/.bin/biome check --write <paths>` (the "schema version does not match" line is informational). Delete `web/.vitest/` if a run creates it.
- Rust checks: `cargo test -p <crate> --all-features <filter>`, `cargo clippy -p <crate> --all-targets --all-features -- -D warnings`, `cargo fmt -p <crate>`.
- AGENTS.md rules apply: no `unwrap`/`expect` outside tests, no placeholders, no compatibility shims, errors through the crate's error types, no payload content in logs or error messages, tests for every behaviour change.
- The UI root font size is 13px: use explicit pixel text sizes (`text-[13px]`, `text-[12px]`, `text-[11px]`).
- Final report: files changed, tests added, check results, anything not done.

## Decisions (already made, do not revisit)

| # | Decision |
|---|---|
| SL-1 | `SessionSummary` gains `error_trace_count: i64` (traces of the session with `status = 'error'`), `first_input_preview: Option<String>` (the `input_preview` of the session's earliest-starting trace) and `last_output_preview: Option<String>` (the `output_preview` of its latest-starting trace). Ties: earliest/latest by `start_time`, then `trace_id`. |
| SL-2 | These come from a separate **extras** query over the listed sessions, like `user_extras` in `crates/storage/src/reads/users.rs`: the traces in the request's scope (window + default exclusions) whose `session_id` is in the page's ids. The sessions **SQL view is not changed** (spec 05 keeps its columns). The session list's filter applies to sessions, so the extras query does not apply it. |
| SL-3 | `GET /sessions` and `GET /sessions/{id}` both return the new fields. |
| SL-4 | UI columns: `first_message` (default visible, fills the remaining width), `last_answer` (hidden by default), `error_rate` (`error_trace_count / trace_count`, red when > 0); Traces, Tokens and Cost get inline bars (`BarCell`, as in the Users list); Last activity shows relative time ("2 h ago") with the exact time in a tooltip; durations over 48 h read as days and hours ("13d 7h"). None of the new columns is sortable. |
| SL-5 | A summary strip and a chart above the list, computed from the **loaded rows** (the filter applies to sessions, so `/metrics` cannot reproduce it): Sessions (count, "+" when more pages exist), Avg turns, Avg duration, Error rate (sum of `error_trace_count` / sum of `trace_count`), Cost, Tokens; and a bar chart "Sessions started" by time bucket over the list's range. When more pages exist the strip says "of N loaded". |
| SL-6 | Demo sessions are realistic: a session's traces start within minutes of each other (most sessions 2–60 minutes long, a few up to 6 hours). |

## Waves

- **Wave A (parallel):** SS1 (storage extras) ∥ SS3a (demo timing).
- **Wave B:** SS2 (server fields, regeneration).
- **Wave C (parallel):** SS3b (mock fields) ∥ SS4 (columns) ∥ SS5 (strip and chart).
- **Wave D:** coordinator: spec 09/11 lines, `just check-web`, `just check-rust` for the touched crates, browser.

---

## SS1 — Storage: session extras

**Edit only:** `crates/storage/src/reads/sessions.rs`, `crates/storage/src/reads/mod.rs` and `crates/storage/src/lib.rs` (re-exports only).

1. Read `user_extras` / `UserExtrasRequest` / `metadata_of` in `reads/users.rs`: follow the same shape (request struct, timeout, `MAX_PAGE` guard, empty ids → empty result, scoped traces restricted with `in_list`, ranking with `row_number`).
2. Add `SessionExtrasRequest<'a> { scope: ListScope, session_ids: &'a [&'a str], timeout: Duration }` and `pub async fn session_extras(ctx, request) -> Result<Vec<RecordBatch>, RunError>` returning one row per session that has traces in scope, columns (export the names as constants): `session_id` (Utf8), `error_trace_count` (Int64, count of traces with `status = 'error'`), `first_input_preview` (Utf8, nullable), `last_output_preview` (Utf8, nullable), per SL-1.
3. Tests next to the existing session tests (use the same fixture/context helpers the file's tests use): counts and previews for a session with several traces (check the first/last choice and the tie rule), a session whose traces have no previews (nulls), unknown ids → no rows, the scope's window excludes traces outside it, `MAX_PAGE + 1` ids → `InvalidRequest`.

**Done when:** `cargo test -p cetra-storage --all-features sessions` and clippy pass.

## SS2 — Server: new `SessionSummary` fields

Runs after SS1. **Edit only:** `crates/server/src/api/sessions.rs`, `crates/server/src/api/convert.rs`, `crates/server/src/routes/sessions.rs`, `docs/spec/09-api.md`, plus what `just openapi` / `just api-examples` regenerate and insta snapshots of these routes.

1. `SessionSummary` gets the three SL-1 fields (doc comments; `error_trace_count` defaults to 0 in the struct building, the previews are `skip_serializing_if = "Option::is_none"`).
2. `convert.rs`: a decoder for the extras batches (follow `user_extras` decoding: `req_*` helpers, errors as `ConvertError`).
3. `routes/sessions.rs`: `attach_session_extras(ctx, sessions, scope, deadline)` called in `list_sessions` (with the list's scope and the shared `Deadline`) and in `get_session` (the session's own scope as `get_session` builds it; if `get_session` has no scope, use the same exclusions the detail uses and say so). Sessions without an extras row keep 0 / none.
4. Route test: listing returns the fields with values matching the fixture (compute expected values from the fixture data, not by copying output).
5. `just openapi`, `just api-examples`; update the route's insta snapshots with `INSTA_UPDATE=always` only if the diff is just the new fields (otherwise STOP).
6. Spec 09: in the `GET /sessions` row, add "each session also has `error_trace_count`, `first_input_preview` (earliest trace's input preview) and `last_output_preview` (latest trace's output preview)".

**Done when:** `cargo test -p cetra-server --all-features sessions`, the `api_examples` integration test and clippy pass; `web/src/api/schema.gen.ts` has the fields.

## SS3a — Demo timing (wave A)

**Edit only:** `web/src/mocks/generated/dataset.ts` and `dataset.test.ts`.

1. Read how traces get their `start_time` and `session_id`. Make the traces of one session start within a realistic span (SL-6): turn k starts 30 s – 8 min after turn k−1 (deterministic from the existing seeded generator), so most sessions last 2–60 minutes and a few up to 6 hours. Traces keep their own durations. If start times are generated before sessions are assigned, STOP and ask how to reorder rather than restructuring.
2. Keep the existing one-topic-per-session and follow-up behaviour, the dataset size, and the time range the dataset covers (sessions simply start at different times across it).
3. Tests: for a few sessions, consecutive turns are 30 s – 8 min apart and the session duration is under 7 hours.

## SS3b — Mock fields (wave C)

Runs after SS2. **Edit only:** `web/src/mocks/generated/dataset.ts`, `handlers.ts` and their tests.

1. The generated sessions (`sessionsFrom` or wherever `SessionSummary` rows are built) fill `error_trace_count`, `first_input_preview`, `last_output_preview` per SL-1 from the session's traces.
2. Tests: a session's first/last previews match its earliest/latest trace previews; `error_trace_count` counts error traces.

## SS4 — Columns (wave C)

Runs after SS2. **Edit only:** `web/src/components/session-columns.tsx` (+ test), `web/src/lib/search/sessions.ts` (+ test), `web/src/lib/format.ts` (+ test, only for the duration change).

1. Column ids: add `first_message`, `last_answer`, `error_rate` to `SESSION_COLUMN_IDS` (order: session, first_message, start, last, traces, user, duration, tokens, cost, errors, error_rate, last_answer, then scores as today; check how score columns are inserted and keep that).
2. `first_message`: the `first_input_preview` text, truncated in one line, full text on hover (`title` or the lazy tooltip used by the trace list's input/output previews — reuse theirs), `size` large (e.g. 360) and the column takes the remaining width: read how `DataTable` builds the grid template; if a column can't flex, STOP and ask. `last_answer`: same with `last_output_preview`, hidden by default.
3. `error_rate`: as the Users list's error-rate cell (read `user-columns.tsx`), from `error_trace_count / trace_count`.
4. Inline bars on Traces, Tokens, Cost: `BarCell` (`@/components/user/bar-cell`) against the max of the loaded rows, exactly as the Users list does.
5. Last activity: relative time (`formatRelative` in `@/lib/format`; check how `TimeText` uses it) with the exact date-time in the tooltip/`title`.
6. Duration: in `formatDuration` (`web/src/lib/format.ts`), values of 48 h or more read as `Nd Hh` (e.g. "13d 7h"); below that unchanged. Update its tests; check other callers' tests still pass.
7. Tests: the new columns render their values; first_message is in the default set and last_answer is not; bars render; duration formatting at 47h59m and 48h and 13d 7h.

## SS5 — Summary strip and chart (wave C)

Runs after SS2. **Edit only:** new `web/src/components/session/sessions-summary.tsx` (+ test), new `web/src/components/session/sessions-started-chart.tsx` (+ test), and `web/src/routes/$org/$project/sessions/index.tsx` (+ test).

1. `SessionsSummary({ sessions, more })`: the SL-5 KPIs computed from the loaded rows, laid out like `UsersSummary` (`web/src/components/user/users-summary.tsx`; read it and reuse its markup and `formatKpi`-style formatting where it fits). "Sessions" shows the count with "+" when `more`; when `more`, a muted "of N loaded" note.
2. `SessionsStartedChart({ sessions, from, to })`: a 64px-high bar chart like `TracesHistogram` (`web/src/components/trace/traces-histogram.tsx`; reuse its chart config and tick formatting helpers) with the count of sessions by start-time bucket over the list's resolved range (24 buckets, or the bucket logic the histogram uses if it is reusable). Bars in `var(--chart-1)`. No drag-to-zoom.
3. Route: render the strip and the chart between the toolbar and the table, like the Users route renders its summary; pass the list's rows, whether more pages exist, and the resolved range.
4. Tests: KPI values from a small set of rows (including the "+" and "of N loaded" case); the chart renders one bar per non-empty bucket; the route shows the strip and the chart.
