# AS M1 · Conversations — plan

Track AS milestone M1 (design: [`00-design.md`](00-design.md), D-AS-1, D-AS-4, D-AS-10, D-AS-12). It adds the assistant's conversation store to `meta.db`, the permission `ProjectPerm::UseAssistant`, the conversation routes (list, create, read, rename, delete; author only, session only), and an hourly job that deletes conversations idle for 30 days and stubs tool results older than 24 hours. Nothing calls a model yet: M3 appends messages from turns, so M1 has no message writer (tests insert messages with raw SQL). Haiku implements the phases one at a time; each phase ends with `just check-rust` green and one commit.

**Prerequisites:** the design commit (`docs/plans/as-assistant/00-design.md`). Tracks D, G, I and P are merged on `main`.

**Read first:**
- `AGENTS.md` (rules: no placeholders, no `unwrap` outside tests, privacy of logs, tests with every change);
- `docs/plans/as-assistant/00-design.md` § Tables, § API, D-AS-4, D-AS-10, D-AS-12;
- `crates/server/src/meta/mod.rs` (`MetaDb::write`, `MetaError`, `unique_or`, the schema snapshot test `schema_matches_the_snapshot`);
- `crates/server/src/meta/dashboards.rs` and `crates/server/src/routes/dashboards.rs` (the closest store and route pair: per-project rows, user ids, `Page`);
- `crates/server/src/auth/roles.rs` (`ProjectPerm`, `min_role`, `key_scope`, the test `permissions_follow_the_spec_table`);
- `crates/server/src/http/access.rs` (`RouteDef`, `Access::Project { perm, session_only }`, `routes()`), `crates/server/src/http/access_suite.rs`;
- `crates/server/src/api/openapi.rs` (`collect_schemas`, `paths`, `dashboard_paths`), `crates/server/tests/openapi.rs` (`OPERATIONS`, `TAGS`, the error-code branches, the session-only security `matches!` blocks, `pages`);
- `crates/server/src/api_examples.rs` and `crates/server/tests/api_examples.rs`, `fixtures/api/README.md` § Fixed identifiers;
- `crates/server/src/serve.rs` (`spawn_purge` and the `tasks` vector);
- `crates/server/src/test_support/mod.rs` (`TestApp`, `seed`, `add_member`, `capture_logs`, `seed_examples`).

## What it is, and what it is not

When M1 is done:
- `meta.db` has `assistant_conversations` and `assistant_messages` (migration `0023_assistant.sql`).
- A project member or above, signed in with a session, can create, list, read, rename and delete their own conversations under `/api/v1/projects/{project}/assistant/conversations`. Nobody else (other members, admins, owners, instance admins) can see them: another user's conversation id is a `404`. PATs and API keys get `403`.
- A conversation is deleted with its project, its org or its author (foreign keys), 30 days after its last activity, or by its author. At most 100 per user per project: creating the 101st deletes the least recently active one.
- Tool results in stored messages are replaced by a stub 24 hours after they were stored.
- OpenAPI, `fixtures/api` and specs 04 and 09 describe it.

Out of scope:
- the org setting, `GET /projects/{p}/assistant`, turns, feedback, titles set from the first message (M3);
- the tool registry and draft contents (M2);
- the web mirror `canUseAssistant` in `web/src/lib/roles.ts` (M4, with its first caller: no unused code before then), the panel and the docs page (M4);
- a `CHANGELOG.md` line (M4 adds one line for the whole feature).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AS-4 | `ProjectPerm::UseAssistant`: minimum role member, no key scope; every assistant route is session only |
| D-AS-10 | Conversations private to their author in one project; 30 days after last activity; 100 per user per project; 200 messages each |
| D-AS-12 | Tool results stubbed 24 h after their turn |
| D-AS-14 | The page context of a user message is stored with it (shape checked in M3) |

Plan-level choices (no design change):
- The table has no `org_id`: projects already cascade from orgs (the design's table list is updated with this plan).
- `ProjectPerm::UseAssistant` is added here; track V adds `RunPlayground` to the same enum and the same spec 08 table. Whichever branch lands second rebases and keeps both.
- A new conversation has an empty title; M3 sets it from the first user message. The UI shows an empty title as "New chat" (M4). A rename needs 1–80 characters after trimming.
- The list is not paged: at most 100 rows, answered as a `Page` whose `next_cursor` is always `null` (the D-I-42 pattern).

## Versions

No new dependency.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| AS1.1 Store | `crates/server/migrations/meta/0023_assistant.sql` (new), `crates/server/src/meta/assistant.rs` (new), `crates/server/src/meta/mod.rs`, `crates/server/tests/snapshots/meta_schema.snap`, `docs/spec/04-metadata-db.md` | — |
| AS1.2 Expiry job | `crates/server/src/assistant/mod.rs` (new), `crates/server/src/lib.rs`, `crates/server/src/serve.rs` | AS1.1 |
| AS1.3 Routes | `crates/server/src/auth/roles.rs`, `crates/server/src/meta/assistant.rs`, `crates/server/src/http/access_suite.rs`, `docs/spec/08-auth.md`, `crates/server/src/api/assistant.rs` (new), `crates/server/src/api/mod.rs`, `crates/server/src/routes/assistant.rs` (new), `crates/server/src/routes/mod.rs`, `crates/server/src/http/access.rs`, `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `crates/server/src/test_support/mod.rs`, `fixtures/api/**`, `fixtures/api/README.md`, `docs/spec/09-api.md` | AS1.2 |

The phases run one after the other in the same worktree, one commit each. AS1.1's store is used outside tests only from AS1.2 (expiry) and AS1.3 (routes), so AS1.1 puts `#![cfg_attr(not(test), allow(dead_code))]` at the top of `meta/assistant.rs` with the comment `// used by the expiry job (AS1.2) and the routes (AS1.3); AS1.3 removes this line`, and AS1.3 removes it.

**Checks for every phase.** Do not build or test while editing. Before the commit, run once:

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
just check-rust
```

`just check-rust` must pass. AS1.3 also runs `just openapi`, `just api-examples` and `just check-web` (it regenerates `web/src/api/schema.gen.ts`).

---

### AS1.1 Store

**Goal:** the two tables and their store functions, with caps, expiry and stubbing, tested.

**Edit:** `crates/server/migrations/meta/0023_assistant.sql` (new), `crates/server/src/meta/assistant.rs` (new), `crates/server/src/meta/mod.rs`, `crates/server/tests/snapshots/meta_schema.snap`, `docs/spec/04-metadata-db.md`.

**Steps:**

1. Create `crates/server/migrations/meta/0023_assistant.sql` with exactly:

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

2. In `crates/server/src/meta/mod.rs`, add `pub(crate) mod assistant;` to the alphabetical module list, after `pub(crate) mod api_keys;`.

3. Create `crates/server/src/meta/assistant.rs`. Follow `meta/dashboards.rs` for style (private `sqlx::FromRow` record structs with `String` ids, `fn corrupt`, `fn parse_uuid`, SQL text only from constants, values always bound). Public items, exactly:

   ```rust
   //! Assistant conversations and messages (track AS, D-AS-10, D-AS-12).
   #![cfg_attr(not(test), allow(dead_code))] // used by the expiry job (AS1.2) and the routes (AS1.3); AS1.3 removes this line

   use cetra_core::ids::ProjectId;
   use sqlx::{SqliteConnection, SqlitePool};
   use uuid::Uuid;

   use super::MetaError;

   /// Conversations a user keeps per project; creating one more deletes the least recently active (D-AS-10).
   pub(crate) const CONVERSATIONS_PER_USER: i64 = 100;
   /// Tool messages stubbed per call of [`stub_tool_results`].
   pub(crate) const STUB_BATCH: i64 = 500;

   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct ConversationRow {
       pub(crate) id: Uuid,
       pub(crate) title: String,
       pub(crate) message_count: i64,
       pub(crate) created_at: i64,
       pub(crate) last_active_at: i64,
   }

   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum MessageRole { User, Assistant, Tool }          // stored "user" | "assistant" | "tool"

   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum MessageStatus { Complete, Cancelled, Failed }  // stored "complete" | "cancelled" | "failed"

   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum Feedback { Up, Down }                          // stored "up" | "down"

   /// A stored message. `parts` and `context` are JSON text, parsed by the route (AS1.3).
   #[derive(Clone, PartialEq, Eq)]
   pub(crate) struct MessageRow {
       pub(crate) seq: i64,
       pub(crate) role: MessageRole,
       pub(crate) parts: String,
       pub(crate) context: Option<String>,
       pub(crate) status: MessageStatus,
       pub(crate) feedback: Option<Feedback>,
       pub(crate) turn_trace_id: Option<String>,
       pub(crate) created_at: i64,
   }

   pub(crate) async fn insert_conversation(conn: &mut SqliteConnection, id: Uuid, project: ProjectId, user: Uuid, now: i64) -> Result<ConversationRow, MetaError>;
   pub(crate) async fn list_conversations(pool: &SqlitePool, project: ProjectId, user: Uuid) -> Result<Vec<ConversationRow>, MetaError>;
   pub(crate) async fn get_conversation(pool: &SqlitePool, project: ProjectId, user: Uuid, id: Uuid) -> Result<Option<ConversationRow>, MetaError>;
   pub(crate) async fn rename_conversation(conn: &mut SqliteConnection, project: ProjectId, user: Uuid, id: Uuid, title: &str) -> Result<Option<ConversationRow>, MetaError>;
   pub(crate) async fn delete_conversation(conn: &mut SqliteConnection, project: ProjectId, user: Uuid, id: Uuid) -> Result<bool, MetaError>;
   pub(crate) async fn list_messages(pool: &SqlitePool, conversation: Uuid) -> Result<Vec<MessageRow>, MetaError>;
   pub(crate) async fn expire_conversations(conn: &mut SqliteConnection, idle_before: i64) -> Result<u64, MetaError>;
   pub(crate) async fn stub_tool_results(conn: &mut SqliteConnection, now: i64) -> Result<u64, MetaError>;
   ```

   `MessageRow` gets a hand-written `Debug` that prints `seq`, `role`, `status`, `parts_len` (`parts.len()`) and `has_context`, never `parts` or `context` (they hold user text and payload content; `AGENTS.md` § Privacy).

   The message writer (`append_message`, its `NewMessage`, the 200-message cap and the enums' `as_str`) comes with M3, its first caller. Each enum has a private `fn parse(text: &str) -> Result<Self, MetaError>` that returns `MetaError::Corrupt { table: "assistant_messages", column: "role" | "status" | "feedback" }` for anything else.

4. Bodies, in order of operations:
   - `insert_conversation`: (1) `SELECT COUNT(*) FROM assistant_conversations WHERE project_id = ? AND user_id = ?`; (2) while the count is `>= CONVERSATIONS_PER_USER`, delete the least recently active one: `DELETE FROM assistant_conversations WHERE id = (SELECT id FROM assistant_conversations WHERE project_id = ? AND user_id = ? ORDER BY last_active_at ASC, id ASC LIMIT 1)` and subtract one (a loop, so a table that somehow holds more than the cap comes back to it); (3) `INSERT` with `title = ''`, `message_count = 0`, `created_at = last_active_at = now`; (4) return the row read back inside the same connection (a private `get_conversation_tx(conn, project, user, id)` with the same `WHERE` as `get_conversation`), or `corrupt("assistant_conversations", "id")` when it is missing.
   - `list_conversations`: the caller's rows of that project, `ORDER BY last_active_at DESC, id DESC`.
   - `get_conversation`: `WHERE id = ? AND project_id = ? AND user_id = ?`. Another user's or another project's id gives `None`.
   - `rename_conversation`: `UPDATE … SET title = ? WHERE id = ? AND project_id = ? AND user_id = ?`; `None` when no row changed; otherwise the row read back. It does not change `last_active_at`. The caller has validated the title.
   - `delete_conversation`: `DELETE … WHERE id = ? AND project_id = ? AND user_id = ?`; `true` when a row went. Messages go by cascade.
   - `list_messages`: `WHERE conversation_id = ? ORDER BY seq ASC`. Callers check ownership first with `get_conversation`.
   - `expire_conversations`: `DELETE FROM assistant_conversations WHERE last_active_at < ?`; return `rows_affected`.
   - `stub_tool_results`: (1) select `conversation_id, seq, parts` from `assistant_messages WHERE stub_after IS NOT NULL AND stub_after <= ? ORDER BY stub_after LIMIT STUB_BATCH`; (2) for each row, parse `parts` as `Vec<serde_json::Value>`; for every element that is an object with `"type": "tool_result"`, set `"result"` to `null` and `"stubbed"` to `true` (insert the keys when absent; leave every other key and element untouched); serialize back with `serde_json::to_string`; when `parts` does not parse as a JSON array, write `[]` instead and log `tracing::warn!(conversation_id = %id, seq, "assistant message parts did not parse; cleared")`; (3) `UPDATE assistant_messages SET parts = ?, stub_after = NULL WHERE conversation_id = ? AND seq = ?`; (4) return the number of rows updated. It never fails on bad JSON, so the job cannot stall on one row.

5. In `docs/spec/04-metadata-db.md`, add a section `## Assistant` after `## Dashboards` (before `## Datasets & experiments`) with: one sentence ("Conversations of the in-app assistant (track AS, D-AS-10): private to their author in one project."), the SQL of step 1 in a `sql` block, and this text:

   > `parts` is a JSON array of parts. The message parts of spec 01 are used where they fit: `{type: "text", content}` and `{type: "tool_call", id, name, arguments}`. Two parts are the assistant's own: `{type: "tool_result", id, name, ok, result, size_bytes, rows?, stubbed}` (the tool's JSON answer, `null` once stubbed) and `{type: "draft", id, kind, draft}` (`kind` is `filter`, `sql`, `dashboard`, `evaluator` or `online_rule`; D-AS-6). A user message holds text parts and its page context in `context`; an assistant message holds text, tool calls and drafts; a tool message holds tool results.
   >
   > Limits: 100 conversations per user and project (creating one more deletes the least recently active), 200 messages per conversation (enforced when turns append them, M3). A conversation is deleted 30 days after `last_active_at`; tool results are stubbed from `stub_after`, 24 hours after their turn (D-AS-12). Both run hourly. Conversations are deleted with their project, org or author.

6. Regenerate the schema snapshot: `INSTA_UPDATE=always cargo test -p cetra-server --lib schema_matches_the_snapshot`, then read the diff of `crates/server/tests/snapshots/meta_schema.snap`: it must add exactly the two tables and three indexes.

**Tests** (in `meta/assistant.rs` `mod tests`, `#[tokio::test(flavor = "multi_thread")]`, using `crate::meta::test_db()` and inserting an org, two projects and two users the way `meta/dashboards.rs`'s tests do). Messages are inserted by a test helper `async fn insert_message(conn: &mut SqliteConnection, conversation: Uuid, seq: i64, role: &str, parts: &str, context: Option<&str>, stub_after: Option<i64>, now: i64)` that runs a raw `INSERT INTO assistant_messages (conversation_id, seq, role, parts, context, stub_after, created_at) VALUES (…)`. Activity is moved with a raw `UPDATE assistant_conversations SET last_active_at = ? WHERE id = ?`. Every `insert_conversation` in a test gets its own increasing `now` (never rely on UUIDv7 order):
- `insert_then_get_round_trips`: every field; title `''`, count 0, `created_at == last_active_at == now`.
- `get_is_scoped_to_owner_and_project`: user B and project 2 get `None` for user A's conversation.
- `list_is_newest_activity_first`: three conversations created at 10, 20, 30; the first moved to 40 by the raw `UPDATE`; order is first, third, second; two rows with the same `last_active_at` come by id desc; another user's rows are absent.
- `the_101st_conversation_evicts_the_least_recently_active`: create 100 at `now` 1..=100, move the first to 1,000 with the raw `UPDATE`, create one more at 1,001: the second-created conversation is gone, the first stays, the user has 100. Another user's conversation in the same project is untouched.
- `rename_changes_only_the_title`: `last_active_at` unchanged; `None` for another user.
- `delete_cascades_messages`: insert two messages, delete; `list_messages` is empty; `false` on a second delete and for another user.
- `list_messages_is_in_seq_order`: messages inserted with seq 2, 1, 3 come back 1, 2, 3, with `context` only where set.
- `messages_round_trip_every_enum`: each role, status and feedback value inserted raw reads back as its enum.
- `a_bad_enum_value_is_corrupt`: `MessageStatus::parse("x")`, `MessageRole::parse("x")` and `Feedback::parse("x")` return `MetaError::Corrupt` with their column (the table's `CHECK` keeps such values out of real rows).
- `expire_deletes_only_idle_conversations`: conversations active at 10, 20 and 30; `expire_conversations(conn, 25)` returns 2 and leaves the third, with its messages.
- `stub_rewrites_only_due_tool_results`: a tool message with parts `[{"type":"tool_result","id":"c1","name":"query_sql","ok":true,"result":{"rows":[1]},"size_bytes":12,"rows":1,"stubbed":false}, {"type":"text","content":"x"}]` and `stub_after = 100`; a second with `stub_after = 200`; a user message with no `stub_after`. `stub_tool_results(conn, 150)` returns 1; the first message's tool result has `result: null`, `stubbed: true`, its other keys unchanged, the text part unchanged, `stub_after` NULL; the second is unchanged. A second call at 150 returns 0.
- `stub_clears_unparseable_parts`: parts `"not json"` → `[]`, `stub_after` NULL, returns 1.
- `stub_takes_at_most_a_batch`: 501 due tool messages (raw inserts; the 200-message cap is not enforced by the store) → the first call returns 500, the second 1.
- `cascades_from_project_org_and_user`: deleting the project (`DELETE FROM projects WHERE id = ?`), then in a second setup the user, then the org, removes their conversations and messages.
- `message_debug_hides_content`: `format!("{row:?}")` of a `MessageRow` whose parts and context contain `"secret-text"` does not contain it.

**Done when:** `just check-rust` passes; the snapshot diff adds only the new tables and indexes.

**Commit:** `Add the assistant conversation store (AS M1)`

---

### AS1.2 Expiry job

**Goal:** the hourly job that deletes idle conversations and stubs due tool results.

**Edit:** `crates/server/src/assistant/mod.rs` (new), `crates/server/src/lib.rs`, `crates/server/src/serve.rs`.

**Steps:**

1. `crates/server/src/lib.rs`: add `pub(crate) mod assistant;` after `pub(crate) mod alerts;` and before `pub mod api;`.

2. Create `crates/server/src/assistant/mod.rs`:

   ```rust
   //! The in-app assistant (track AS). M1: conversation expiry and tool-result stubbing (D-AS-10, D-AS-12).

   use std::time::Duration;

   use tokio::task::JoinHandle;

   use crate::http::AppState;
   use crate::meta::MetaError;
   use crate::meta::assistant::{STUB_BATCH, expire_conversations, stub_tool_results};

   /// A conversation idle this long is deleted (D-AS-10): 30 days.
   pub(crate) const CONVERSATION_TTL_NS: i64 = 30 * 24 * 60 * 60 * 1_000_000_000;
   /// How often the job runs.
   const EXPIRY_EVERY: Duration = Duration::from_secs(60 * 60);

   /// What one run removed.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
   pub(crate) struct ExpiryOutcome { pub(crate) expired: u64, pub(crate) stubbed: u64 }

   /// One run: deletes the conversations idle for [`CONVERSATION_TTL_NS`], then stubs every due tool result,
   /// one batch of [`STUB_BATCH`] per write transaction.
   pub(crate) async fn expire_once(state: &AppState) -> Result<ExpiryOutcome, MetaError>;

   /// Runs [`expire_once`] every hour; the first run is one interval after start.
   pub(crate) fn spawn_expiry(state: AppState) -> JoinHandle<()>;
   ```

   `expire_once`: (1) `now = state.clock.now_ns()`; (2) one `state.meta.write` calling `expire_conversations(conn, now.saturating_sub(CONVERSATION_TTL_NS))`; (3) loop: one `state.meta.write` calling `stub_tool_results(conn, now)`, add the count, stop when the count is below `STUB_BATCH`; (4) return the outcome.

   `spawn_expiry`: the shape of `serve.rs`'s `spawn_purge`: `tokio::time::interval(EXPIRY_EVERY)`, `MissedTickBehavior::Skip`, the first `tick` consumed before the loop; each tick runs `expire_once`; `Ok` with anything removed logs `tracing::info!(expired, stubbed, "assistant conversations expired")`; `Err` logs `tracing::error!(error = %error, "assistant expiry failed")` and the loop goes on.

3. `crates/server/src/serve.rs`: push `crate::assistant::spawn_expiry(state.clone())` into the `tasks` vector, after `spawn_last_used(state.clone())`. It also runs in restore mode (local database work only).

**Tests** (in `assistant/mod.rs` `mod tests`, with `TestApp::new()`, `app.seed()`, the store's `insert_conversation` inside `app.state.meta.write`, and the manual clock `app.state.clock.set(…)`):
- `expires_idle_conversations`: a conversation created at `t`, another whose `last_active_at` is moved to `t + 10 days` by a raw `UPDATE`; set the clock to `t + 30 days + 1 ns`; `expire_once` returns `expired: 1`; the second is still listed.
- `keeps_a_conversation_active_exactly_at_the_limit`: last activity at exactly `now - CONVERSATION_TTL_NS` stays (the comparison is `<`).
- `stubs_when_due`: a tool message inserted raw (as in AS1.1's tests) with `stub_after = t + 86_400_000_000_000`; with the clock one ns before, `expire_once` stubs nothing; at `stub_after` it stubs 1, and the stored parts have `result: null, stubbed: true`. (`STUB_AFTER_NS`, the 24 hours, comes with M3, which sets `stub_after`.)
- `stubs_more_than_a_batch_in_one_run`: 501 due tool messages inserted raw; one `expire_once` returns `stubbed: 501`.

**Done when:** `just check-rust` passes.

**Commit:** `Expire assistant conversations and stub old tool results (AS M1)`

---

### AS1.3 Routes

**Goal:** the permission and the five conversation routes, documented, with examples.

**Edit:** `crates/server/src/auth/roles.rs`, `crates/server/src/api/assistant.rs` (new), `crates/server/src/api/mod.rs`, `crates/server/src/routes/assistant.rs` (new), `crates/server/src/routes/mod.rs`, `crates/server/src/http/access.rs`, `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `crates/server/src/test_support/mod.rs`, `fixtures/api/**`, `fixtures/api/README.md`, `docs/spec/09-api.md`.

**Steps:**

0. `crates/server/src/meta/assistant.rs`: remove the `#![cfg_attr(not(test), allow(dead_code))]` line added in AS1.1. Every remaining item is now used outside tests.

1. `crates/server/src/auth/roles.rs`: add `UseAssistant` as the last variant of `ProjectPerm`, with the doc comment `/// Chat with the in-app assistant and manage one's own conversations (D-AS-4).` In `min_role`, add it to the `Member` arm. In `key_scope`, add it to the `None` arm. In the test `permissions_follow_the_spec_table`, assert `ProjectPerm::UseAssistant.min_role() == Role::Member` and `ProjectPerm::UseAssistant.key_scope() == None`, next to the `ManageDashboards` asserts.

2. Create `crates/server/src/api/assistant.rs` (and `pub mod assistant;` in `api/mod.rs`, after `pub mod alerts;`). Types, exactly (derives as the other API modules: `Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema`; request types add `#[serde(deny_unknown_fields)]`):

   ```rust
   /// A conversation in the list (D-AS-10).
   pub struct AssistantConversation {
       pub id: Uuid,
       /// Empty until the first message or a rename; the UI shows "New chat".
       pub title: String,
       pub message_count: u32,
       #[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]
       pub created_at: i64,
       #[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]
       pub last_active_at: i64,
   }

   /// A conversation with its messages, oldest first.
   pub struct AssistantConversationDetail {
       pub id: Uuid,
       pub title: String,
       pub message_count: u32,
       #[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]
       pub created_at: i64,
       #[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]
       pub last_active_at: i64,
       pub messages: Vec<AssistantMessage>,
   }

   #[serde(rename_all = "snake_case")]
   pub enum AssistantRole { User, Assistant, Tool }

   #[serde(rename_all = "snake_case")]
   pub enum AssistantMessageStatus { Complete, Cancelled, Failed }

   #[serde(rename_all = "snake_case")]
   pub enum AssistantFeedback { Up, Down }

   #[serde(rename_all = "snake_case")]
   pub enum AssistantDraftKind { Filter, Sql, Dashboard, Evaluator, OnlineRule }

   /// One part of a message (spec 04 § Assistant).
   #[serde(tag = "type", rename_all = "snake_case")]
   pub enum AssistantPart {
       Text { content: String },
       ToolCall { id: String, name: String, #[schema(value_type = Object)] arguments: serde_json::Value },
       ToolResult {
           id: String,
           name: String,
           ok: bool,
           /// The tool's answer; `null` once stubbed (D-AS-12).
           #[schema(value_type = Option<Object>)]
           result: Option<serde_json::Value>,
           size_bytes: u64,
           #[serde(default, skip_serializing_if = "Option::is_none")]
           rows: Option<u64>,
           stubbed: bool,
       },
       Draft { id: String, kind: AssistantDraftKind, #[schema(value_type = Object)] draft: serde_json::Value },
   }

   pub struct AssistantMessage {
       pub seq: u32,
       pub role: AssistantRole,
       pub parts: Vec<AssistantPart>,
       /// The page context sent with a user message (D-AS-14).
       #[serde(default, skip_serializing_if = "Option::is_none")] #[schema(value_type = Option<Object>)]
       pub context: Option<serde_json::Value>,
       pub status: AssistantMessageStatus,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub feedback: Option<AssistantFeedback>,
       /// The turn's trace, 32 lowercase hex characters.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub turn_trace_id: Option<String>,
       #[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]
       pub created_at: i64,
   }

   /// `PATCH …/assistant/conversations/{id}`.
   #[serde(deny_unknown_fields)]
   pub struct UpdateAssistantConversationRequest {
       /// 1–80 characters after trimming.
       pub title: String,
   }
   ```

   If utoipa cannot derive `ToSchema` for an attribute written above, use the form the existing modules use for the same case (`api/dashboards.rs`, `api/datasets.rs`) and report it.

3. Create `crates/server/src/routes/assistant.rs` with `pub(crate) fn routes() -> Vec<RouteDef>` (and `pub(crate) mod assistant;` in `routes/mod.rs`, in alphabetical order; `routes.extend(crate::routes::assistant::routes());` in `http/access.rs` `routes()`, after the alert-rule routes). In `http/access_suite.rs` nothing changes: the path parameter is `{id}`, which `concrete_path` already replaces. Every route: `access: Access::Project { perm: ProjectPerm::UseAssistant, session_only: true }`, `documented: true`.

   | Method | Path | Handler | Answer |
   |---|---|---|---|
   | GET | `/api/v1/projects/{project}/assistant/conversations` | `list_conversations` | `200 Page<AssistantConversation>`, `next_cursor: null` |
   | POST | `/api/v1/projects/{project}/assistant/conversations` | `create_conversation` (no body) | `201 AssistantConversation` |
   | GET | `/api/v1/projects/{project}/assistant/conversations/{id}` | `get_conversation` | `200 AssistantConversationDetail` |
   | PATCH | same | `update_conversation` | `200 AssistantConversation` |
   | DELETE | same | `delete_conversation` | `204` |

   Handlers:
   - Extractors as in `routes/dashboards.rs`: `State(state): State<AppState>`, `Extension(principal): Extension<Principal>`, `Extension(access): Extension<ProjectAccess>`, `error::Path(ConversationPath { id }): error::Path<ConversationPath>`, `error::Json(body): error::Json<UpdateAssistantConversationRequest>` (the `error::` wrappers from `http/error.rs`, never axum's own). Get the user with `principal::session_user(&principal)?`; the project is `access.project.id`.
   - `#[derive(Debug, Deserialize)] struct ConversationPath { id: String }`; an `id` that does not parse as a `Uuid` is `ApiError::NotFound` (not a 400: the id space is private).
   - Store calls: reads on `state.meta.reader()`, writes inside `state.meta.write(move |conn| Box::pin(async move { … }))`. `create` uses `Uuid::now_v7()` and `state.clock.now_ns()`. `get`, `update` and `delete` give `ApiError::NotFound` when the store answers `None`/`false`.
   - `get_conversation` reads the row, then `list_messages`, and converts each `MessageRow` with `fn message_json(row: &MessageRow) -> Result<AssistantMessage, ApiError>`: `parts` parsed as `Vec<AssistantPart>` and `context` as `serde_json::Value`; a parse failure is `ApiError::from(MetaError::Corrupt { table: "assistant_messages", column: "parts" })` (or `"context"`), a 500. `seq` and `message_count` convert with `u32::try_from`, failing the same way.
   - `update_conversation` trims the title; empty or more than 80 characters (`chars().count()`) returns `Err(ApiError::BadRequest { detail: None, errors: vec![FieldError { field: "title".to_owned(), message: "must be 1 to 80 characters".to_owned(), position: None }] })` (`FieldError` from `crate::api::common`).
   - Audit lines, ids only (never the title): `tracing::info!(target: "audit", conversation_id = %id, project_id = %project, user_id = %user.id, "assistant conversation created")`, likewise `renamed` and `deleted`.

4. OpenAPI (`api/openapi.rs`): a tag `tag("assistant", "The in-app assistant's conversations, private to their author (D-AS-10).")`; register the schemas (`AssistantConversation`, `AssistantConversationDetail`, `AssistantMessage`, `AssistantPart`, the four enums, `UpdateAssistantConversationRequest`, and `insert_page::<AssistantConversation>("AssistantConversationPage")`); add `fn assistant_paths(paths: PathsBuilder) -> PathsBuilder` chained in `paths()`, modelled on `dashboard_paths`, with operation ids `list_assistant_conversations`, `post_assistant_conversation`, `get_assistant_conversation`, `patch_assistant_conversation`, `delete_assistant_conversation`, the path parameter `path_string("id", "Conversation id.")`, the description constant `ASSISTANT_ACCESS = "Project members and above, with a session; only the conversation's author (D-AS-4, D-AS-10)."`, and these error sets: list, get, delete `[401, 403, 404, 429]`; post `[401, 403, 404, 429]`; patch `[400, 401, 403, 404, 429]`. Each of the five operations gets `.securities(Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]))`, as `post_org_sso_domain` does.

5. `crates/server/tests/openapi.rs`: add the five rows to `OPERATIONS`, `"assistant"` to `TAGS`, `("AssistantConversationPage", "AssistantConversation")` to `pages`, and the five ids to the session-only `matches!` list that ends with `delete_project`. In `document_lint`'s error-code chain, add `patch_assistant_conversation` to the branch that holds `get_alert_rule` and `delete_alert_rule` (it expects `400, 401, 403, 404, 429`); the four others need no entry, since they fall to the final branch (`401, 403, 404, 429`). Do not add any of the five to `QUERY_OR_BODY` or `READ_OPERATIONS`, which change the expected set. Then run `just openapi` to rewrite `crates/server/openapi.json` and `web/src/api/schema.gen.ts`.

6. Examples. In `crates/server/src/test_support/mod.rs`, add `pub(crate) const EXAMPLE_CONVERSATION: &str = "01900000-0000-7000-8000-000000001a01";` and `pub(crate) async fn seed_example_conversation(&self)`. It must run after `seed_examples` (which rewrites the example project's id). It reads ada's id with `SELECT id FROM users WHERE email = 'ada@example.com'`, inserts the conversation with `meta::assistant::insert_conversation(conn, EXAMPLE_CONVERSATION id, EXAMPLE_PROJECT id, ada, NOW)`, sets the title `"Why did checkout errors spike?"` with `rename_conversation`, and inserts four messages with raw `INSERT`s (fixed times one second apart, `seq` 1–4), then sets `message_count = 4` and `last_active_at` to the last time with a raw `UPDATE`:
   1. user: `[{"type":"text","content":"Why did checkout errors spike yesterday?"}]`, context `{"page":"traces","filter":"status = 'error'","range":"24h"}`;
   2. assistant: `[{"type":"tool_call","id":"call_1","name":"query_sql","arguments":{"query":"SELECT name, count(*) AS errors FROM traces WHERE status = 'error' GROUP BY name ORDER BY errors DESC LIMIT 5","from":"2026-01-01T00:00:00Z","to":"2026-01-02T00:00:00Z"}}]`;
   3. tool: `[{"type":"tool_result","id":"call_1","name":"query_sql","ok":true,"result":{"columns":[{"name":"name","type":"VARCHAR"},{"name":"errors","type":"BIGINT"}],"rows":[{"name":"checkout","errors":3}]},"size_bytes":120,"rows":1,"stubbed":false}]`, `stub_after` 24 hours after its time;
   4. assistant: `[{"type":"text","content":"Most errors come from the checkout trace (3 of them)."},{"type":"draft","id":"draft_1","kind":"filter","draft":{"view":"traces","filter":"name = 'checkout' AND status = 'error'","range":"24h"}}]`, `turn_trace_id` `"0af7651916cd43dd8448eb211c80319c"`.

   In `api_examples::generate()`, call it after `seed_examples`, then `fetch` + `out.add` for `list_assistant_conversations` (`responses/assistant-conversations.json`) and `get_assistant_conversation` (path param `("id", EXAMPLE_CONVERSATION)`, `responses/assistant-conversations/<EXAMPLE_CONVERSATION>.json`). No example for the POST, PATCH or DELETE answers (`manifest_covers_the_read_operations` needs only GETs). Add a request example `fixtures/api/requests/assistant-conversation-rename.json` (`{"title": "Checkout error spike"}`): an entry `("patch_assistant_conversation", "requests/assistant-conversation-rename.json")` in the `requests` array of `crates/server/tests/api_examples.rs`, and `roundtrip::<UpdateAssistantConversationRequest>("requests/assistant-conversation-rename.json")` next to the `requests/score-batch.json` round trip. In `generated_files_roundtrip`'s `match` on the operation id (the one ending in `other => panic!("{other} has no round-trip")`), add `"list_assistant_conversations" => roundtrip::<Page<AssistantConversation>>(&example.file)` and `"get_assistant_conversation" => roundtrip::<AssistantConversationDetail>(&example.file)`. Run `just api-examples` to write the files and the manifest. Add the id range `…1a01–…1a02` (assistant conversations) to `fixtures/api/README.md` § Fixed identifiers.

7. `docs/spec/08-auth.md`: in the permission table (`| Action | viewer | member | admin | owner |`), add after the "Manage evaluators, …" row: `| Use the in-app assistant: chat and manage one's own conversations, with a session only (D-AS-4) | | ✓ | ✓ | ✓ |`.

8. `docs/spec/09-api.md`: add a section `### Assistant` after `### Evaluation & alerts` with a two-column table (`Method & path | Notes`):
   - `GET/POST /assistant/conversations` · `GET/PATCH/DELETE /assistant/conversations/{id}` — the caller's own conversations, private to their author (another user's id is `404`); project members and above with a session (`ProjectPerm::UseAssistant`, D-AS-4); the list is newest activity first and not paged (at most 100); `POST` takes no body; `PATCH` takes `{title}` (1–80 characters); the detail holds the messages, oldest first, with their parts (spec 04 § Assistant) (D-AS-10).

**Tests** (in `routes/assistant.rs` `mod tests`, with `TestApp::new()` and `app.seed()`. A member or viewer is `app.add_member(&seeded, "bob@example.com", OrgRole::Member | OrgRole::Viewer)`, which returns `Member { id, cookie }`; a second project of the same org is inserted with `meta::projects::insert_project` inside `app.state.meta.write`. Messages are inserted with raw SQL as in AS1.1):
- `member_creates_lists_reads_renames_and_deletes`: the full cycle with a member's cookie; the list holds the conversation after create, the rename, then nothing after delete; status codes 201, 200, 200, 200, 204.
- `viewer_is_forbidden`: a viewer gets 403 on all five routes.
- `pat_and_key_are_forbidden`: `app.personal_token(seeded.user_id, true)` (read-write PAT) and `seeded.read_key` (`Authorization: Bearer …`) get 403 on list and create.
- `another_users_conversation_is_not_found`: user B (member) gets 404 on get, patch and delete of A's conversation; A's list is unchanged; an org owner gets 404 too.
- `other_project_is_not_found`: A's conversation id under another project of the same org is 404.
- `bad_id_is_not_found`: `/conversations/not-a-uuid` is 404.
- `rename_validates_the_title`: `""`, `"   "` and 81 characters are 400 with field `title`; 80 characters pass; surrounding spaces are trimmed in the answer.
- `rename_rejects_unknown_fields`: `{"title":"x","extra":1}` is 400.
- `detail_returns_messages_in_order`: three messages inserted with raw SQL (user with context, assistant with a tool call and a draft, tool with a result) come back in seq order with every field, `context` only on the first.
- `corrupt_parts_are_an_internal_error`: a raw `UPDATE` setting `parts = 'x'` makes the detail 500.
- `audit_logs_hold_no_title`: under `capture_logs`, create and rename to `"secret title"`; the log has `assistant conversation renamed` and the conversation id, not `secret title`.
- the access suite (`every_route_against_every_principal`) passes without change, since the routes declare their access.

**Done when:** `just check-rust` and `just check-web` pass (the regenerated `schema.gen.ts` must type-check); `just openapi` and `just api-examples` leave no diff after a second run.

**Commit:** `Add the assistant conversation routes (AS M1)`
