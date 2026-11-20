# AQ · Annotation queues — design

Track AQ adds annotation queues: named lists of traces, spans or sessions that project members work through one item at a time, labelling each with the annotation form (D-I-37) restricted to the queue's score definitions. D192 moved them into the MVP (amending D20). This document is the high-level design; each milestone gets its own plan, written from the code that exists when it starts.

Decisions are `D-AQ-<n>` in `docs/mvp-decisions.md` § Track AQ. "(user)" marks the user's choices of 2026-11-20.

## Goals

- A project member creates a queue: a name, an optional description and reviewer instructions, one target kind (trace, span or session), and an ordered list of the project's score definitions.
- Items get in from a trace, span or session page, from checked rows of the traces, spans and sessions lists, from everything a list's filter matches, and through the REST API and the Python and TypeScript `Client`.
- A reviewer opens a queue and works through it: the next pending item, the real trace or session view, the queue's instructions, a form with the queue's definitions, then Complete (and next), Skip (with an optional reason) or Next.
- Two reviewers never get the same item at the same time (a lease), and each item is done once (one reviewer per item).
- Scores written from a queue carry the queue's id, so results can be filtered and queried per queue.
- Deleting a trace, a user's data or a project removes the matching queue items; retention and the data window apply to items.
- The Free plan gets one queue per org; the other plans have no limit.

## Non-goals

- **N reviewers per item** (Opik's `annotators_per_item`) (user). An item is done when one reviewer completes or skips it. Re-opening it lets someone else redo it.
- **Assignment** (user). Any member works any queue of the project: no reviewer list, no per-item assignee and no "my items" inbox.
- **Online rules that route items into a queue** (user's selection left them out). Items come only from people and from the API.
- **A per-queue results page** (user's selection left it out). The scores carry `queue_id`, so the scores list, the trace lists' score columns, the SQL console and dashboards can filter by it. A Results tab can come later without a data change.
- **Comments on items.** The score's `reason` is the free text.
- **Dataset items as targets** (MLflow and Laminar). Queues hold project data only.
- **MCP tools.** The MCP server keeps its read-only tools; a later track may add a read tool for queues.

## What competitors do

Read in their code (`/home/sebastien/github/…`; never copied):

| | Langfuse | Opik | LangWatch | MLflow | Laminar |
|---|---|---|---|---|---|
| Target | trace, observation, session, mixed in a queue | trace or thread, one per queue | trace | trace | span snapshot |
| Score definitions | ordered id list per queue | list per queue | junction | junction | inline schema |
| Done | per item | per reviewer, N per item | per item | per item, plus `declined` | per item |
| Assignment | cosmetic | none | members, personal items | user pool | none |
| Lock | 5 min, non-atomic | Redis lease of 300 s, heartbeat | none | none | none |
| Bulk add | filter as a background job | ≤ 1 000 ids, automation | traces × queues | idempotent | idempotency key |
| Trace deleted | orphan item | ? | ? | items deleted | n/a (snapshot) |
| Plan gate | 1 or 3 queues on low plans | none | none | none | none |

We take:
- Opik's one kind per queue.
- Langfuse's and Opik's ordered definitions per queue and the queue id on scores.
- MLflow's third end state (our `skipped`) and its cascade on trace deletion.
- An atomic lease with a heartbeat, which Langfuse lacks (its check-then-write races).
- A database-enforced unique item per target. Langfuse has none, so its `skipDuplicates` is a no-op.

## Architecture

```
  web: queue pages, reviewer page, "Add to queue" dialogs
        │  REST (session, PAT, sk write key)
        ▼
  cetra-server ── routes/annotation_queues.rs ── meta/annotation_queues.rs ── meta.db
        │              │                                     (queues, definitions, items)
        │              ├─ adds by id: storage trace/span/session lookups (visibility, guard)
        │              ├─ adds by filter: storage id scans (traces, spans, sessions), capped
        │              └─ entitlement check (max_annotation_queues)
        ├─ scores::write ── validates `queue_id` against meta.db ── storage `scores.queue_id`
        ├─ deletion/resolve.rs ── deletes the items of the deletion scope (same transaction)
        └─ retention ── deletes the items whose target is older than keep_from
```

Queues and items live in `meta.db`: they are few, mutable and need atomic claims. Scores stay in the columnar store, with one new nullable column.

### Data model (`meta.db`, one migration, M2)

- **`annotation_queues`**
  - Columns: `id` (UUIDv7); `project_id` (FK, cascade); `name` (1–100 characters, unique per project); `description` (≤ 1 000 characters, optional); `instructions` (Markdown, ≤ 10 000 characters, optional); `target` (`trace` | `span` | `session`, immutable after create); `created_by_user_id` (FK, set null); `created_at`; `updated_at`; `version` (optimistic concurrency, as dashboards).
  - At most 100 queues per project. The 101st is `409 conflict`, as for dashboards.
  - A queue never moves between projects. `DELETE` removes the row (no archive), so the plan limit counts live rows.
- **`annotation_queue_definitions`**
  - Columns: (`queue_id` FK cascade, `definition_id` FK to `score_definitions`, `position`).
  - Each queue has 1–50 definitions, in order, each at most once. They must be live when they are added.
  - Score definitions are only ever archived (D-I-26), never removed, so the foreign key never breaks.
  - An archived definition stays on the queue, but the reviewer form leaves it out. A queue whose definitions are all archived shows a notice instead of the form.
- **`annotation_queue_items`**
  - **Columns:**
    - `id` (UUIDv7), `queue_id` (FK, cascade) and `project_id`.
    - `trace_id` (hex, set for trace and span items), `span_id` (span items) and `session_id` (session items).
    - `target_time`: read at add time, and used by retention and the data window.
      - Trace and span items: the trace's start time.
      - Session items: the start of the session's earliest trace visible at add time (the data window applies; the list's `[from, to)` range does not).
    - `status` (`pending` | `completed` | `skipped`) and `skip_reason` (≤ 1 000 characters).
    - `leased_by_user_id` and `done_by_user_id` (FK to users, set null), `lease_expires_at` and `done_at`.
    - `added_by_kind` (`user` | `pat` | `key`) and `added_by_id`. The id has no foreign key, so it survives a revoked key.
    - `created_at`, `updated_at`.
  - A unique index on (`queue_id`, `trace_id`, `span_id`, `session_id`), with `coalesce` over the nullable columns.
  - Indexes:
    - (`queue_id`, `status`, `id`) for the claim;
    - (`project_id`, `trace_id`) and (`project_id`, `session_id`) for the deletion cascade and the "In n queues" lookup;
    - (`project_id`, `target_time`) for retention.
  - At most 100 000 items per queue.
  - Counts (`pending`, `completed`, `skipped`) are computed per request with one `GROUP BY` over the claim index. They are not stored.

### Score column (M1)

- `scores.queue_id`: Utf8, nullable, the queue that produced the score.
  - It is a field of `cetra-core`'s `Score` and `ScoreCreate`, a column of the Arrow schema and the storage `scores` table, of the `scores` SQL view and the filter DSL, and a `queue_id` filter of `GET /scores`.
  - It is set by whoever writes the score: the reviewer form sends it, and the API may send it.
- **Not part of the derived score id** (D-AQ-6). The derived id stays `(project, target, name, source, author)` (spec 06). A reviewer's session-written (`human`) score of name N on target T is one row whether it was written from the trace view or from a queue, and the latest write sets `queue_id`.
  - Annotating the same trace in two queues with the same definition keeps one score, tagged with the second queue.
  - Writes through a PAT store `api` (D-I-36), so they are another row, as today.
- **Format.** Adding the column is a storage format change. Before the first public release there is no format constraint (D185): no `FORMAT_VERSION` bump or migration, unless a release is tagged before M1 lands. M1 regenerates the site's SQL schema reference (`just docs-gen`), and the golden test directory is rebuilt by the tests.
- **The check.** M1 accepts the field unchecked. From M2, `scores::write` checks that a given `queue_id` names a queue of the project (`400 invalid_request`, field `queue_id`). It does not check the target kind or the definition list: a queue id on a score is a tag.

### API contract (project routes `/api/v1/projects/{p}/…`)

| Method & path | Notes |
|---|---|
| `GET /annotation-queues` · `POST /annotation-queues` | list (by name, cursor; each row carries `pending`, `completed`, `skipped` counts; `?target_trace_id=` or `?target_session_id=` keeps the queues holding an item of that target, trace and span items both matching a `target_trace_id`); create `{name, description?, instructions?, target, definition_ids}` |
| `GET/PATCH/DELETE /annotation-queues/{id}` | `PATCH {version, name?, description?, instructions?, definition_ids?}` (target immutable; `409 conflict` on a stale version); `DELETE` removes the queue and its items, never scores |
| `GET /annotation-queues/{id}/items?status=&cursor=` | items by id: target, state, `leased_by_user_id` while the lease is live, `done_by_user_id`, `done_at`, `skip_reason`. User ids only, never names (D-P-67); the web resolves names through the members list |
| `POST /annotation-queues/{id}/items` | `{items: [{trace_id} \| {trace_id, span_id} \| {session_id}]}`, 1–1 000, the queue's kind only; `201 {added, existing, skipped: [{index, message}]}`; existing items keep their state |
| `POST /annotation-queues/{id}/items/matching/preview` · `POST …/items/matching` | the queue kind's list filter body (filter DSL, `search`, `[from, to)`, `include_experiments`); preview `{matched, capped}`; add `{added, existing, capped}`; at most 10 000 targets per call |
| `DELETE /annotation-queues/{id}/items/{item}` | remove one item |
| `POST /annotation-queues/{id}/next` | `{after?: item_id}` → the leased item, or `204` when no pending item is free |
| `POST …/items/{item}/lease` · `DELETE …/items/{item}/lease` | acquire or renew: succeeds when the item is pending and free, expired or the caller's; `409 conflict` with the holder's user id otherwise · release (`204`, also when not held) |
| `POST …/items/{item}/complete` · `…/skip {reason?}` · `…/reopen` | state changes; complete and skip release the lease |

**Permissions** (D-AQ-11):
- `ProjectPerm::ManageQueues` covers queue writes, item adds and removals, and both `matching` routes: role `member`, an `sk` key with `write`, a read-write PAT.
- `ProjectPerm::Review` covers `next`, lease, complete, skip and reopen: role `member`, and **no API key** (`key_scope` none, as `ManageEvaluation`), because a lease and `done_by_user_id` name a person. A read-write PAT is accepted.
- Reads need `View`. A viewer opens queues and items read-only.

**Errors:**
- An add that would take the queue past 100 000 items is `422 too_many_items`, and nothing is added. `existing` targets do not count toward the cap.
- A target of the wrong kind is listed in `skipped`.

### Adding items

- **By id** (the dialogs and the API).
  - The server looks the batch up in storage in one read per call, with the project's visibility (the data window).
  - It finds the trace start for each trace or span, or each session's `target_time`.
  - These targets are listed in `skipped` with their index (D-J-25's shape): one not found or hidden, an evaluation trace (D-I-18), or one blocked by the deletion guard (D-M-6).
  - The guard holds trace ids and end-user ids. A trace or span item is checked by its `trace_id`. A session item is checked by the session's visible trace ids, which the lookup returns, and the session is skipped when any of them is guarded, so it matches the cascade below.
  - The insert runs in one meta transaction, which checks the guard again inside it, as D-J-9 does. An item can then never be added after a deletion stored its scope.
  - Existing targets are counted, not changed (D-AQ-8).
- **By filter.** A synchronous call in two steps, preview then add, as "Delete matching…" (D-M-5):
  - The storage scans are asked for 10 001 targets, so `capped` can be told apart from exactly 10 000.
  - Every scan returns each target with its `target_time`:
    - the trace scan: `filter_trace_ids` (it already takes a `limit`, a window and `include_eval: false`), extended to return each trace's start time;
    - a new span scan: (`trace_id`, `span_id`, trace start);
    - a new session scan: (`session_id`, earliest visible trace start, the session's visible trace ids for the guard check).
  - Like the deletion scan, which targets come back when capped is unspecified. The answer and the UI say "narrow the filter to add the rest".
  - The scans use the project's data window (not `Exempt`), and they never include evaluation traces: the body has no `include_eval`.
  - The insert re-checks the guard in its transaction, as above.

### Reviewer flow and the lease (D-AQ-4)

- **Claim.** `POST …/next` runs one `UPDATE … RETURNING` on the single writer connection. It picks the queue's first pending item, ordered by id, that is free (no lease, or an expired one) or already held by the caller. With `after`, it takes the first such item past that id, then wraps around to the start once. The claim sets `leased_by_user_id` and `lease_expires_at = now + 5 min`.
  - The writer `Mutex` makes this atomic, so two reviewers never get the same item.
  - A caller holds at most one lease per queue: `next` and the lease route release the caller's other lease in that queue in the same transaction.
  - Times come from the server's `Clock` (`clock.rs`), so tests drive expiry.
- **Heartbeat.** The reviewer page renews the lease every 60 s while the item is open, and releases it on leave (best effort); an unreleased lease expires after 5 minutes.
  - When a renewal answers `409`, the page turns the item read-only with "Another reviewer took this item" and offers Next.
  - A removed member's lease simply expires. Their user id is set null by the foreign key if the user is deleted.
- **Complete / Skip.** Allowed to the lease holder, or to anyone with `Review` when no live lease is held by another reviewer; otherwise `409 conflict`. Both set `done_by_user_id` and `done_at` and clear the lease. Completing does not check the scores, so an item can be completed with no score (D-AQ-5).
- **Save, then complete.** Scores live in storage and items in `meta.db`, so they cannot commit together. "Complete and next" first saves the form's changed rows (`POST /scores` with `queue_id`), then completes the item.
  - A failed save completes nothing.
  - A failed complete after a good save leaves the scores and the item pending, and shows the error.
- **Reopen.** Sends a completed or skipped item back to `pending`. Any `Review` holder can reopen; this is how a second opinion happens.
- **Opening an item by link** (from the items table, or Previous):
  - A pending item that is free or the caller's: the page takes the lease.
  - A pending item another reviewer holds: read-only, "Being reviewed by <name>".
  - A completed or skipped item: no lease. A `Review` holder can still edit their own scores in the form and reopen the item.
  - Viewers and API-key holders never lease.
- **No live updates.** The SSE bus carries spans and `resync` only (D-G-12), and queues add no event. The queue pages refetch their counts and items every 10 s while visible and on window focus, and after every action.

UI, in `/$org/$project/annotation-queues`:
- **The queue list:** name, kind, a progress bar (completed + skipped / total) and pending count, and Start reviewing.
- **The queue page:** a header with Start reviewing; tabs for Items (status filter, add and remove) and Settings (name, description, instructions, definitions; delete).
- **The reviewer page:** `/annotation-queues/$id/review/$itemId`.
  - Left: the existing trace view (with the span selected for a span item), or the session view.
  - Right: the instructions (rendered Markdown, collapsible) and the existing annotation form (`annotation-form.tsx`).
    - It is limited to the queue's live definitions and saves with `queue_id`.
    - It keeps D-I-37's rules: the caller's own scores, and the `session_id` and `user_id` read from the loaded target.
  - Buttons: Complete and next (`Ctrl/⌘+Enter`), Next without changing the state (`Ctrl/⌘+Shift+Enter`), Skip (opens a reason box) and Previous (browser history).
  - A counter "n of N done", and an end screen when `next` answers `204`.
- **Entry points:**
  - "Add to queue" in the trace header (trace, or span when the span is selected) and on the session page.
  - "Add to queue" in the bulk-action bar of checked rows on the traces, spans and sessions lists.
  - "Add matching to queue…" on the three lists (next to "Delete matching…" on traces), with a count preview.
  - Each dialog lists only the queues of the matching kind.
- **The trace and session views** show an "In n queues" chip, which links to the queues.
- **Nav:** "Annotation queues" in the sidebar's Evaluate section, after Online rules and before Datasets, with the go key `r` (free in `GO_TARGETS`).

### Deletion, retention and the data window (D-AQ-9)

- **GDPR deletions** (`trace_ids`, `filter`, `user`): the meta write in `deletion/resolve.rs` that stores the scope deletes the scope's queue items in the same transaction, next to the `eval_jobs` delete (M8) and J's cascade (D-J-9).
  - Trace and span items go when their `trace_id` is in the scope.
  - Session items go when their `session_id` is a session of any scope trace.
    - A `user` deletion's scope already lists its sessions.
    - For `trace_ids` and `filter`, today's scope has no session ids. M2 adds one storage read in the resolve step: the distinct non-null `session_id`s of the scope's traces, exempt from the data window like the other deletion reads.
    - A session that loses only some of its traces still loses its queue items. The session id may identify the person, and the reviewed session has changed anyway.
  - Scores are erased by storage, as today.
- **Project and org drops:** the FK cascade.
- **Retention** departs from D-M-10's "retention does not follow the target", for `meta.db` items only. An item is a pointer, not a copy as J's dataset items are, so it has no use once its target is gone.
  - Both `retention_once` (hourly) and `apply_now` (the `PATCH`) delete the project's items with `target_time` before the start of the `keep_from` day, one indexed `DELETE` each. "Keep forever" deletes nothing.
  - A session item goes with its earliest trace's day, even if the session has newer traces.
  - Completed items go too, and the counts drop with them. The scores stay until their own day passes (D-M-10).
- **The data window** (D-P-21): item lists, counts, `next` and the "In n queues" lookup leave out items with `target_time` before `visible_from`, so a downgraded org does not see them, and they come back if it upgrades.
- **A target that cannot be loaded anymore** (a race with a deletion) shows "This trace is no longer available", with Remove from queue.

### Plan limit (D-AQ-10)

- A new limit `max_annotation_queues`, appended to D-P-13's limits, counts an org's queues across its projects.
  - Free sets it to 1 in the control plane's catalog. Self-hosted without a key has no limit.
  - The count and the insert run in one writer transaction, so two concurrent creates cannot both pass.
  - `POST /annotation-queues` past it answers `403 plan_limit` (D-P-14).
  - The UI shows the locked state and an Upgrade button (D-P-15).
  - Downgrading removes nothing; existing queues stay usable.
- The limit is added in each place that carries one:
  - the signed org token's double-option field (`cetra-token`) and its merge in `entitlements/tokens.rs`;
  - the config `entitlements.defaults` and the configuration reference;
  - the server's resolved `Entitlements`, `GET …/entitlements` and the CLI's limits printout.
- The control plane (track Q, private repository) must add it to its plan catalog. That request goes to the coordinator (§ Requests).

## Decisions

| ID | Summary |
|---|---|
| D-AQ-1 | A design doc and five milestones (below). |
| D-AQ-2 | One target kind per queue: trace, span or session, chosen at create and immutable (user). |
| D-AQ-3 | Done is per item: one reviewer completes or skips it; no N-reviewer mode and no assignment (user). |
| D-AQ-4 | An atomic lease on the single writer: 5 min, renewed every 60 s by the reviewer page, at most one per reviewer per queue; complete and skip are refused (409) while another reviewer holds a live lease; "Complete" saves the scores first, then completes; no live updates, the pages refetch every 10 s. |
| D-AQ-5 | Item states `pending`, `completed`, `skipped` (with an optional reason); reopen sends either back to pending; Next leaves the state unchanged (user); completing does not require a score. |
| D-AQ-6 | Scores carry `queue_id` (user): a nullable column in `cetra-core` and storage, set by the writer, checked to name a queue of the project, and not part of the derived score id. |
| D-AQ-7 | Each queue has an ordered list of 1–50 score definitions; the reviewer form shows its live ones, with D-I-37's controls and own-scores rule; and instructions in Markdown (user). |
| D-AQ-8 | Adds by id (1–1 000, D-J-25's `skipped` shape) and by filter (preview then add, at most 10 000, unspecified which when capped, never evaluation traces), with the deletion guard re-checked in the insert transaction, plus the REST API and `Client` methods (user); one item per target per queue, enforced by a unique index; re-adding keeps the item's state. |
| D-AQ-9 | Items are deleted by GDPR deletions (in the resolve transaction; session items by the sessions of the scope's traces, a new read for `trace_ids` and `filter`), drops (FK) and retention by `target_time` (a departure from D-M-10, for these pointers only); the data window hides them. |
| D-AQ-10 | `max_annotation_queues`: Free 1 per org, others unlimited, `403 plan_limit` (user). |
| D-AQ-11 | `ProjectPerm::ManageQueues` (member, `sk` write key, read-write PAT) for queues, item adds and removals and the `matching` routes; `ProjectPerm::Review` (member, no API key, read-write PAT) for `next`, lease, complete, skip and reopen; reads `View`, with user ids only (D-P-67). |

## Milestones

Each milestone is one plan, written when it starts. Each one keeps `just check` green, updates the specs it touches, `openapi.json` and `fixtures/api`, adds a `CHANGELOG.md` line, and updates the docs site's pages for its user-visible part.

**M1 — `queue_id` on scores.**
- **Delivers:** the column through `cetra-core` (`Score`, `ScoreCreate`, Arrow schema and batch conversion), storage (the `scores` table, the SQL view and the filter DSL field), the server's score write and `GET /scores?queue_id=`, the fixtures and specs 01, 05 and 09, and the regenerated SQL schema reference. The field is accepted unchecked until M2 adds the lookup.
- **Depends on:** nothing. The `cetra-core` change needs the coordinator's approval (shared contract).
- **Leaves out:** queues.

**M2 — Queues and items on the server.**
- **Delivers:** the migration; `meta/annotation_queues.rs`; every route of § API contract except the two `matching` routes; the lease; adds by id; the `queue_id` check in `scores::write`; `ManageQueues` and `Review`; the deletion cascade (with the storage read of the scope traces' sessions), retention and data window; the `max_annotation_queues` limit (`cetra-token`, config, entitlements, CLI); specs 04, 08, 09 and 12; the configuration reference; `api_examples.rs` and `fixtures/api`.
- **Depends on:** M1.
- **Leaves out:** filter adds, UI and SDK.

**M3 — Adds by filter.**
- **Delivers:** the start time in `filter_trace_ids`'s answer, new storage scans for span targets and session targets (each with its `target_time`, sessions with their trace ids) by the spans and sessions lists' filters, with the cap and the data window; the two `matching` routes.
- **Depends on:** M2.
- **Leaves out:** UI.

**M4 — Web UI.**
- **Delivers:** the nav entry, the queue list and queue pages, the reviewer page with the lease heartbeat and shortcuts, the "Add to queue" dialogs (single, checked rows, matching), the "In n queues" chip, the Free plan's locked state, MSW mocks and tests, spec 11, the docs site's annotation-queues page, and its screenshots.
- **Depends on:** M2 and M3, through their API contract in `openapi.json`. M4 can start from M2's contract while M3 is implemented, if the matching dialog comes last.
- **Leaves out:** a results tab.

**M5 — SDK `Client` methods.**
- **Delivers:** `client.annotation_queues` in Python and TypeScript: `list`, `get`, `create`, `update`, `delete`, `add_items` (trace ids, span pairs or session ids) and `items`; their tests against `fixtures/api` and in `just e2e`; spec 10; the SDK reference pages.
- **Depends on:** M2.
- **Leaves out:** the reviewer actions, which need a person.

Order: M1 → M2 → (M3 ∥ M5) → M4. M1 and M3 both touch `cetra-storage`, so they never run at the same time.

## Requests to other tracks (§ Requests)

- **Coordinator:** approve the `cetra-core` change of M1 (`Score.queue_id`, `ScoreCreate.queue_id`, a column of the `scores` Arrow schema).
- **Track Q (control plane):** add `max_annotation_queues` to the plan catalog (Free 1, others unset) and to the tokens it signs, once M2 lands the token field.
- **Track IC (issue clustering) and AS (assistant):** neither needs anything from AQ; an issue's traces could later be sent to a queue through `POST …/items`.

## Open questions for the user

1. **Caps.** 100 queues per project, 100 000 items per queue, 1 000 ids per add and 10 000 per filter add. Are these the right sizes?
2. **Retention deletes items (D-AQ-9).** When retention drops a trace's day, its queue items go too, completed ones included, so a queue's done count can shrink. The alternative keeps them as "no longer available" rows. Is deleting right?
3. **One score per reviewer per target, whatever the queue (D-AQ-6).** The same trace annotated with the same definition in two queues keeps one score, tagged with the latest queue. The alternative puts `queue_id` in the derived id: one score per queue, but then the trace view's Annotate tab shows several scores of the same name by the same person. Is the trade-off right?
4. **Viewers.** Can a viewer open a queue and see its items read-only (current design: yes, `View`), or should queues be hidden from viewers?
