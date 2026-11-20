# IC M4 · Links — plan

M4 connects issues to the rest of the product (D-IC-14, amended by D-IC-20 to D-IC-25):
- K gets a fifth alert condition, `issue_occurrences`, over one issue or one source;
- each issue source can notify K's channels of new and regressed issues, in one digest message per 10 minutes;
- an issue page can copy its newest occurrence traces into a dataset, and can open a prefilled alert rule;
- MCP gets a read-only `list_issues` tool;
- the real binary and the in-process tests cover the whole chain.

**Prerequisites:** M1, M2 and M3 are committed on the branch this one is cut from. Migration `0024` is M3's, so M4's migration is `0025`. M3 writes `created` and `regressed` issue events in its assign path (03 plan § M3.4c); notifications read the `issue_events` table, so they cover M1's error issues and M3's LLM issues alike.

**Read first:**
- `AGENTS.md`;
- `00-design.md` (D-IC-9 to D-IC-11, D-IC-14, D-IC-16, D-IC-18) and the decisions this plan adds (`ic-m4-decisions` summary in M4.1);
- the code M1 and M3 added: `crates/server/src/meta/issues.rs`, `issues/`, `routes/issues.rs`, `api/issues.rs`;
- track K: `crates/server/src/alerts/{mod,condition,evaluate,deliver,message,channel}.rs`, `meta/alerts.rs`, `routes/alert_rules.rs`, `routes/channels.rs`, `api/alerts.rs`, `email/mod.rs`, `email/templates/alert.*`;
- track J: `routes/datasets.rs` (`add_from_traces`, `item_from`), `api/datasets.rs`;
- track H: `mcp/{mod,tools,sql,output}.rs`;
- storage: `crates/storage/src/reads/{alerts,scores}.rs`;
- web: `web/src/lib/alerts.ts`, `components/alerts/rule-form.tsx`, `components/issues/issue-actions.tsx`, `components/datasets/add-traces-to-dataset-dialog.tsx`, `components/issue-sources/source-form.tsx`, `routes/$org/$project/{alerts/new,issues/$issueId}.tsx`, `mocks/writes.ts`.

## Open questions for the coordinator

The decisions file (D-IC-20 to D-IC-25) leaves these open. Each phase is written with the **assumption** shown; change the assumption and the phase text together.

| # | Question | Assumed (most conservative) | Phase |
|---|---|---|---|
| Q1 | Extra fields of the `issues.detected` webhook body beyond `{source_id, source_name, project, items, more, issues_url}` | add `type` (`"issues.detected"`), `notification_id` and `occurred_at`, mirroring the alert payload's `type`, `event_id`, `occurred_at`; items hold `{issue_id, kind, title}` only (no per-item URL) | M4.7 |
| Q2 | Which issues does "Add examples to dataset" read for a group (the issue and its merged issues)? | the issue, then the 7 merged issues with the most occurrences over the default occurrences window (14 days ending now), as the occurrences examples rank them (`example_issues`, 8 names at most); `limit` rows are read per name | M4.10a, M4.11 |
| Q3 | Answer when the issue group has no occurrence to add | `400` with the field error `issue`: "the issue has no occurrences in the data window" | M4.11 |
| Q4 | Existing enabled sources when migration `0025` runs (none in practice: the project is unreleased) | the migration sets `last_notified_at` to the migration time for every enabled source, so none is silent forever and none sends a backlog | M4.1 |
| Q5 | The "View traces" link and button of an `issue_occurrences` alert message | the plain trace list over the event window, without a filter (the rule has none) | M4.4 |
| Q6 | Preview chart of an `issue_occurrences` rule in the rule form | none (`previewQuery` returns `undefined`, as it does for LLM-span latency) | M4.12a |
| Q7 | Titles in Slack and email lines | cut to 100 characters (as rule names are); the webhook carries the whole title | M4.7 |
| Q8 | An issue event written with `at` earlier than a source's `last_notified_at` after that value was set (a write that committed late) | never sent (D-IC-21); reduced by the settle margin `ISSUE_NOTIFY_SETTLE_NS` (60 s): the tick collects only events with `at <= now - margin` | M4.3 |
| Q9 | Singular wording (`1 new or regressed issues`) | singular for one (`1 new or regressed issue`), plural otherwise, in Slack, email subject and body | M4.7 |
| Q10 | A `created` or `regressed` event of an issue that is ignored or merged by the time of the tick | still notified (no filter on the issue's state); accepted | M4.3 |
| Q11 | Window of an `issue_occurrences` rule | `15m` or more; `1m` and `5m` are refused (decided by the coordinator) | M4.4 |

**Coordinator's answers (binding; they override the assumptions and any phase text that follows them):**
- Q1: as assumed.
- Q2: the dataset route reads the same 8 names as the occurrences examples after the M1 fixes: the issue plus the 7 merged issues with the most occurrences (`example_issues`, built on `most_occurrences_first`, both moved to `issues/read.rs` by M4.10a), counted over the default occurrences window (`OCCURRENCE_DEFAULT_PERIOD_NS`, 14 days, ending now). It picks the most recent occurrences across those names, one per trace, up to `limit`.
- Q3: as assumed, with the message "the issue has no occurrences in the data window".
- Q4: as assumed.
- Q5: as assumed.
- Q6: as assumed.
- Q7: as assumed.
- Q8: mostly as assumed, with a settle margin so a late commit is not lost: the tick collects only events with `at <= now - ISSUE_NOTIFY_SETTLE_NS` (a const, 60 s, with a doc comment saying why), and `last_notified_at` advances to the newest collected event's `at`. Test: an event newer than the margin waits for the next tick. D-IC-21's text in M4.1 says so.
- Q9: singular and plural: "1 new or regressed issue" / "N new or regressed issues", in Slack, email subject and email body. Test both.
- Q10: as assumed. Q11: as stated (decided by the coordinator): `issue_occurrences` refuses windows `1m` and `5m` with 400 `condition.window`: "issue occurrences need a window of 15m or more".

## What it is, and what it is not

When M4 is done:
- an alert rule can count the occurrences of one issue or of a whole source (`issue_occurrences`), and fires, resolves and notifies like every K rule;
- a source with channels sends one digest per 10 minutes at most, of its new and regressed issues, to Slack, a webhook or email;
- "Add examples to dataset" and "Create alert" work from the issue page;
- `list_issues` works over MCP;
- the CLI e2e covers error issues end to end, and in-process tests cover alerts and notifications with the manual clock.

Out of scope:
- annotation queues (M5);
- notifications for issue state changes made by people, renames and merges (never notify);
- a channel test-send for issue messages (the channel test stays the alert sample, D-IC-21);
- a per-issue notification subscription (the list is per source).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IC-9 | Merge: counts and names of an issue group; at most 1 000 names per read |
| D-IC-10, D-IC-11, D-IC-18 | Lifecycle, occurrence scores (`issue:<id>`, source `issue`, `cetra.issue.span_id` in the metadata, `created_at` = occurrence time) |
| D-IC-14 | Links (amended by D-IC-20 to D-IC-25) |
| **D-IC-20** | `issue_occurrences` condition; storage `AlertMeasure::ScoreCount`; names resolved per evaluation; chunks of 1 000 |
| **D-IC-21** | `issue_source_channels`, `issue_sources.last_notified_at`, `issue_notifications`, the `notification_deliveries` rebuild, `notify_issues`, messages |
| **D-IC-22** | `POST …/issues/{id}/dataset-items`, the shared bulk-add core, metadata `cetra.issue_id` |
| **D-IC-23** | "Create alert" and the prefilled rule form |
| **D-IC-24** | MCP `list_issues` (eighth tool) |
| **D-IC-25** | e2e coverage split: CLI e2e for error issues, in-process for alerts and notifications |
| D-K-2, D-K-4, D-K-5, D-K-7, D-K-8, D-K-14, D-K-19 | conditions, state model, channels, delivery, messages, rule limit, email quota (amended: D-K-2) |
| D-J-4, D-J-5, D-J-8, D-J-9, D-J-25 | dataset limits, permission `ManageDatasets`, deletion guard, bulk add |
| D-H-10, D-H-11 | MCP limits and tools (amended: D-H-11) |

## Versions

No new dependency. (`minijinja`, `serde`, `uuid`, `sqlx` and every web package are already in use.)

## Phases

| Phase | Edits (summary) | Depends on |
|---|---|---|
| M4.1 Decisions, spec 04, migration | `00-design.md`, `docs/mvp-decisions.md`, spec 04, migration `0025` | — |
| M4.2 Storage `ScoreCount` | `crates/storage/src/reads/alerts.rs` | — |
| M4.3 Link store | new `meta/issue_links.rs`, `meta/mod.rs`, new `test_support/issues.rs`, `test_support/mod.rs` | M4.1 |
| M4.4 K condition | `api/alerts.rs`, `alerts/condition.rs`, `alerts/message.rs`, `routes/alert_rules.rs`, `routes/channels.rs`, `meta/alerts.rs` (tests), `alerts/deliver/tests.rs` (literal), `alerts/evaluate.rs` (the `measure` call and test literals) | — |
| M4.5 K evaluation | `alerts/evaluate.rs` | M4.2, M4.3, M4.4 |
| M4.6 Source channels API | `api/issues.rs`, `routes/issues.rs` | M4.3, M4.4 |
| M4.7 Issue messages | `alerts/message.rs`, `email/mod.rs`, two new templates | M4.4 |
| M4.8 Issue deliveries | `meta/alerts.rs`, `alerts/deliver.rs`, `alerts/deliver/tests.rs`, `routes/channels.rs` | M4.3, M4.7 |
| M4.9 `notify_issues` tick | new `alerts/issue_notify.rs`, `alerts/mod.rs` | M4.3 |
| M4.10a Shared issue reads | new `issues/read.rs`, `issues/mod.rs`, `routes/issues.rs` | M4.3, M4.6 |
| M4.10b MCP `list_issues` | new `mcp/issues.rs`, `mcp/tools.rs`, `mcp/mod.rs` | M4.7, M4.10a |
| M4.11 Dataset route | `routes/datasets.rs`, `api/datasets.rs`, new `routes/issue_dataset_items.rs`, `routes/mod.rs`, `http/access.rs`, `api/openapi.rs` | M4.10a |
| M4.12a Web alert lib | `web/src/lib/alerts.ts`, `lib/alerts.test.ts`, `lib/search/alerts.ts` | — |
| M4.12b Web rule form | `rule-form.tsx`, `rule-preview.tsx`, `rule-detail.tsx`, new `channel-checkboxes.tsx`, new `issue-scope-fields.tsx`, new `select-field.tsx`, `alerts/new.tsx`, tests | M4.12a |
| M4.13 Web issue page | `issue-actions.tsx`, `$issueId.tsx`, the dataset dialog, `mocks/writes.ts`, tests | M4.12b |
| M4.14 Web source editor | `source-form.tsx`, tests | M3 (editor), M4.12b |
| M4.15 In-process tests | new `alerts/issue_links_tests.rs`, `alerts/mod.rs` | M4.5, M4.6, M4.8, M4.9, M4.10a |
| M4.16 CLI e2e | `crates/cli/tests/e2e.rs` | M4.11 |
| M4.17 Docs | specs 06, 07, 09, 11, website pages, `CHANGELOG.md`, `docs/roadmap.md` | M4.1 |
| Closing | generated files, checks | all |

**Parallelism** (Edit lists below are disjoint inside each group; groups run when their dependencies are committed):
- Start together: **M4.1, M4.2, M4.4, M4.12a**.
- After M4.1: **M4.3**, **M4.17**.
- After M4.3 and M4.4: **M4.5** (also needs M4.2), **M4.6**, **M4.7** (needs M4.4 only), then **M4.8** (M4.3, M4.7), **M4.9** (M4.3). M4.5, M4.6, M4.8 and M4.9 touch different files and run in parallel.
- After M4.6: **M4.10a**, then **M4.10b** (also needs M4.7) and **M4.11** together.
- After M4.12a: **M4.12b**, then **M4.13** and **M4.14** (both need M4.12b: the prefilled `alerts/new` search and the channel checkboxes).
- Last: **M4.15** (after M4.5, M4.6, M4.8, M4.9), **M4.16** (after M4.11), then the Closing section.
- **M4.4 leaves the crate compiling**: it changes `measure` to return `Option` and fixes its one caller and the `AlertCondition` literals in `evaluate.rs` in the same phase. M4.5 then adds the issue branch. No phase leaves the crate broken for the next.
- Shared Rust test helpers live in `crates/server/src/test_support/issues.rs`, created by M4.3 (the first phase that builds sources, issues and events); every later phase uses them.
- Web phases never need the Rust phases to compile: they are written against the contracts in this plan.

**Rules for every phase** (repeat them in each brief):
- Follow the plan exactly. If a step is unclear, contradicts the code or cannot be done as written, stop and report instead of improvising.
- Touch only the phase's **Edit** files.
- **No compiling or testing during the phases** (the user's rule: the machine is overloaded). The only commands allowed are `rustfmt --edition 2024 <the .rs files you edited>` and, for web files, `pnpm --dir web exec biome format --write <the files you edited>`. Write each test with its phase. Every check runs once, in the Closing section.
- Do not regenerate generated files (`openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`, `fixtures/api/static/*` that tests regenerate, insta snapshots, `website/src/generated/*`, `web/src/mocks/generated/*`, docs-gen pages). They are regenerated at the end.
- No `unwrap()`/`expect()` outside tests unless a comment states the invariant. No payload text (titles, descriptions, summaries, webhook or email content) in logs or error messages: log ids, counts and error classes.
- **Dead code:** a new `pub(crate)` item with no non-test caller yet gets `#[cfg_attr(not(test), allow(dead_code))]`. The Closing section removes every attribute M4 added.
- Re-read the current text of each file before editing it: M3 changed `meta/issues.rs`, `routes/issues.rs`, `api/issues.rs` and `source-form.tsx` after this plan's line numbers were taken. Names and signatures are given in full; line numbers are only hints.
- Commit once, with the given subject, staging only the phase's files (`git add <paths>`). No attribution lines.

---

## M4.1 Decisions, spec 04, migration

**Goal:** the decisions, the schema text and the migration of D-IC-20 to D-IC-25.

**Edit:**
- `docs/plans/ic-issue-clustering/00-design.md`;
- `docs/mvp-decisions.md`;
- `docs/spec/04-metadata-db.md`;
- new `crates/server/migrations/meta/0025_issue_links.sql`.

**Migration** (exact; M4.3 relies on every name):
```sql
-- Track IC M4: issue notifications and links (D-IC-21).
ALTER TABLE issue_sources ADD COLUMN last_notified_at INTEGER;
-- Existing enabled sources start now, so a backlog is never sent (D-IC-21).
UPDATE issue_sources
   SET last_notified_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000000000
 WHERE enabled = 1;
CREATE INDEX issues_source ON issues (source_id);

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

-- SQLite cannot drop NOT NULL: rebuild notification_deliveries. No table references it.
CREATE TABLE notification_deliveries_new (
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
INSERT INTO notification_deliveries_new
    (id, event_id, channel_id, status, attempts, next_attempt_at, last_error, created_at, updated_at)
SELECT id, event_id, channel_id, status, attempts, next_attempt_at, last_error, created_at, updated_at
  FROM notification_deliveries;
DROP TABLE notification_deliveries;
ALTER TABLE notification_deliveries_new RENAME TO notification_deliveries;
CREATE INDEX notification_deliveries_due ON notification_deliveries(status, next_attempt_at);
CREATE INDEX notification_deliveries_event ON notification_deliveries(event_id);
CREATE INDEX notification_deliveries_channel ON notification_deliveries(channel_id, updated_at);
CREATE INDEX notification_deliveries_issue ON notification_deliveries(issue_notification_id);
```
The digest JSON of `issue_notifications.digest` is `{"items":[{"issue_id":"<uuid>","kind":"created"|"regressed","title":"<text>"}],"more":<n>}`: at most 20 items, newest first, `more` is the count of the rest.

**Steps:**
1. Write the migration above verbatim.
2. `00-design.md`: add `### D-IC-20 The issue_occurrences condition` to `### D-IC-25 End-to-end coverage` after `### D-IC-19`, as bullet lists whose text is the decisions summary of D-IC-20 to D-IC-25 (the coordinator's `ic-m4-decisions.md`, already amended with the settle margin, the singular wording, the window rule and the example ranking; copy its text, keep its wording). Then amend the rest of the doc:
   - `### D-IC-14 Links`: replace the *Condition* bullets with "see D-IC-20"; replace the *Notifications* bullets with "see D-IC-21" (no `issue_event_id`); in the *Datasets* bullet replace "`metadata.issue_id`" with "metadata `cetra.issue_id` (D-IC-22)"; add "(D-IC-24)" after "MCP". Remove the sentence naming `alert_count_scores`.
   - `### D-IC-15`, bullet `cetra-storage`: replace "`alert_count_scores` (M4, D-IC-14)" with "`AlertMeasure::ScoreCount` (M4, D-IC-20)".
   - `## Architecture` § Tables: row `issue_sources`: replace "notification channel ids, last_notified_at" with "last_notified_at"; add rows `issue_source_channels` (source, channel) and `issue_notifications` (id, source, digest JSON, created_at).
   - `## Architecture` § API table, row `POST …/issues/{id}/dataset-items`: body `{dataset, limit ≤ 100}`.
   - `## Milestones` row M4: replace "`alert_count_scores` in storage" with "`AlertMeasure::ScoreCount` in storage".
3. `docs/mvp-decisions.md`, section `## Track IC`: add rows `D-IC-20` … `D-IC-25` after `D-IC-19` (one paragraph each, same words as the design sections, in the table's style: `| D-IC-20 | <title> | **<bold summary>** <rest> |`). Edit the `D-IC-14` row: delete "`issue_occurrences` condition (…storage `alert_count_scores`)" detail and "(`notification_deliveries.issue_event_id`, one message per source per 10 minutes)" in favour of "(D-IC-20)" and "(D-IC-21)"; "through J's path" → "through J's bulk-add code (D-IC-22)"; "MCP gets a read-only `list_issues`" → "(D-IC-24)". Edit the `D-IC-15` row: `alert_count_scores` → `AlertMeasure::ScoreCount`. Edit the `D-K-2` row: "**Four conditions, no `group_by`.**" → "**Five conditions, no `group_by`.**"; append before "(User's choice.)": "D-IC-20 adds `issue_occurrences` (`issue_id` or `source_id`, no filter, no `min_count`)." Edit the `D-H-11` row: "**Seven read-only tools:**" → "**Eight read-only tools:**" and add `list_issues` at the end of the list; append "D-IC-24 adds `list_issues`."
4. `docs/spec/04-metadata-db.md`: in § Issues, add to `issue_sources` DDL the column `last_notified_at INTEGER` (comment `-- ns; the last issue notification cut-off (D-IC-21)`), the index `CREATE INDEX issues_source ON issues (source_id);`, and the DDL of `issue_source_channels` and `issue_notifications` exactly as in the migration, with a paragraph (what each holds, the digest JSON, the 10-minute throttle). In § Alerts, replace the `notification_deliveries` DDL by the rebuilt table and its four indexes, and add one sentence: "A delivery belongs to an alert event or to an issue notification, never both (D-IC-21)."

**Tests:** none (docs and SQL). The Closing checks (`just check-rust`) run every migration against the schema tests.

**Done when:** `rustfmt` is not needed. By hand: the migration has every statement above in that order; `grep -rn "alert_count_scores" docs --exclude-dir=plans` finds nothing (the M4 plan itself and older plans are excluded); the D-IC-20 to D-IC-25 headings exist in `00-design.md` and rows in `mvp-decisions.md`; the D-IC-21 text names `ISSUE_NOTIFY_SETTLE_NS` and the D-IC-20 text the `15m` window rule. The migration changes the insta snapshot `crates/server/tests/snapshots/meta_schema.snap`; it is not edited here: the Closing accepts it (step 5, `cargo insta review`).

**Commit:** `Record the IC M4 decisions and add the issue links migration`

---

## M4.2 Storage `AlertMeasure::ScoreCount`

**Goal:** `alert_value` counts the live scores of a name list in a window (D-IC-20).

**Edit:** `crates/storage/src/reads/alerts.rs` (its test module included).

**Contract:**
```rust
pub enum AlertMeasure {
    // …the six existing variants…
    /// The number of live scores whose name is in `names` and whose `created_at` is in the
    /// window, whatever their target (D-IC-20). `AlertValueRequest::filter` must be absent.
    ScoreCount {
        /// Score names, at most [`MAX_COUNT_NAMES`]; empty counts 0.
        names: Vec<String>,
    },
}
```
Result: `AlertValue { count: n, value: Some(n as f64) }` (zero is a value, like `Cost`).

**Steps:**
1. Add the variant (keep `#[derive(Debug, Clone, PartialEq, Eq)]`). Import `MAX_COUNT_NAMES` from `reads/scores.rs` (it is `pub const` there).
2. At the top of `compute`, before the filter is parsed: when the measure is `ScoreCount` and `present(request.filter).is_some()`, return `RunError::InvalidRequest("a score count takes no filter".to_owned())`. The `match &request.measure` in `compute` gets an arm `AlertMeasure::ScoreCount { names }` that calls a new private `async fn score_count(ctx, window, names) -> Result<AlertValue, RunError>`:
   - more than `MAX_COUNT_NAMES` names: `RunError::InvalidRequest(format!("at most {MAX_COUNT_NAMES} names"))`;
   - empty `names`: `Ok(AlertValue { count: 0, value: Some(0.0) })` without a query;
   - else `ctx.session().table("scores")` (the live scores view, as `score_value` does), `.filter(col(scores::NAME).in_list(names as lit, false).and(window.predicate(scores::CREATED_AT)))`, `.aggregate(vec![], vec![count(lit(1_i64)).alias(COUNT)])`, `collect`, then `single_row` and `i64_at(require(batch, COUNT)?, 0)` as the other arms do (copy the error mapping of `score_value`: `classify` for planning errors, `ctx.fail(error)` for the collect).
   No trace join, no default exclusions, no target filter.
3. `crates/storage/src/model_tests/check.rs` has `other => unreachable!(…)` for measures it never reads: leave it.
4. Update the doc comment of `AlertValueRequest` (`window`: "`created_at` for scores") — unchanged — and of `AlertValue::count` ("scores") — unchanged; no edit.

**Tests** (the module's `score(...)` helper builds a trace-target score at fixed times, so build these with `crate::test_support::score_row(id, trace, created_at, updated_at, ingest, deleted, name)` (`crates/storage/src/test_support.rs`), set `.target` for a span or session target, and pass the rows to the module's `context(spans, scores)`; `live_scores_table` applies `live_scores`):
- `score_count_counts_names_in_the_window`: three names, scores inside and outside `[from, to)`; only the listed names and the in-window scores count; `value == Some(count as f64)`.
- `score_count_counts_every_target_and_ignores_traces`: a trace-target, a span-target and a session-target score of one name, with no matching spans or traces in the context: count 3 (no trace filter).
- `score_count_zero_is_a_value`: no match gives `count 0`, `value Some(0.0)`.
- `score_count_with_no_names_is_zero_without_a_query`: empty names give 0.
- `score_count_refuses_more_than_the_name_cap`: `MAX_COUNT_NAMES + 1` names give `RunError::InvalidRequest`; exactly `MAX_COUNT_NAMES` is accepted.
- `score_count_refuses_a_filter`: `filter: Some("status = 'error'")` gives `InvalidRequest`.
- `score_count_skips_deleted_scores`: a score with a newer version of the same id built with `deleted: true` is not counted (the tombstone hides it from the live view).

**Done when:** `rustfmt --edition 2024 crates/storage/src/reads/alerts.rs` succeeds; by hand, every `match` on `AlertMeasure` in this file is exhaustive.

**Commit:** `Add the score count alert measure`

---

## M4.3 Link store

**Goal:** the `meta.db` functions of D-IC-20 and D-IC-21 in one new file.

**Edit:** new `crates/server/src/meta/issue_links.rs`; `crates/server/src/meta/mod.rs` (one line: `pub(crate) mod issue_links;`, in alphabetical place); new `crates/server/src/test_support/issues.rs`; `crates/server/src/test_support/mod.rs` (one line `pub(crate) mod issues;` next to the other `pub(crate) mod` lines).

**Types and functions** (exact signatures; bodies as the steps say). Reuse the conventions of `meta/issues.rs` (`MetaError`, `SqlitePool`/`SqliteConnection`). `corrupt` and `parse_uuid` are private there (and again in `meta/alerts.rs`): define private copies of the same shape in `issue_links.rs`:
```rust
//! Issue links: alert name sets, source channels and issue notifications (D-IC-20, D-IC-21).

/// What one issue notification shows (D-IC-21).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Digest {
    /// At most [`DIGEST_ITEMS`], newest first.
    pub(crate) items: Vec<DigestItem>,
    /// Events that did not fit in `items`.
    pub(crate) more: u64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DigestItem {
    pub(crate) issue_id: Uuid,
    pub(crate) kind: DigestKind,
    pub(crate) title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DigestKind { Created, Regressed }

/// Most items one digest holds (D-IC-21).
pub(crate) const DIGEST_ITEMS: usize = 20;
/// Minimum time between two notifications of one source: 10 minutes (D-IC-21).
pub(crate) const THROTTLE_NS: i64 = 600_000_000_000;
/// Most channels one source notifies (D-IC-21).
pub(crate) const MAX_SOURCE_CHANNELS: usize = 10;
/// A tick collects only events at least this old: 60 seconds. An event's `at` is the time its
/// writer read the clock, and its transaction can commit a little later; without the margin a tick
/// between the two would advance `last_notified_at` past an event it never saw, and the event
/// would never be sent (D-IC-21).
pub(crate) const ISSUE_NOTIFY_SETTLE_NS: i64 = 60_000_000_000;

/// An enabled source of an active project in an active org that can notify now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NotifySource {
    pub(crate) id: Uuid,
    pub(crate) last_notified_at: i64,
}

/// What one notify attempt did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Notified {
    /// A notification row and one pending delivery per channel were written.
    Sent { notification_id: Uuid, items: usize, more: u64, deliveries: usize },
    /// No `created` or `regressed` event since `last_notified_at`; nothing was written.
    NoEvents,
    /// The source changed (or lost its channels) since `due_sources` read it; nothing was written.
    Changed,
}

pub(crate) async fn issue_scope_ids(pool: &SqlitePool, project: ProjectId, issue: Uuid) -> Result<Option<Vec<Uuid>>, MetaError>;
pub(crate) async fn source_scope_ids(pool: &SqlitePool, project: ProjectId, source: Uuid) -> Result<Option<Vec<Uuid>>, MetaError>;
pub(crate) async fn source_channel_ids(pool: &SqlitePool, source: Uuid) -> Result<Vec<Uuid>, MetaError>;
pub(crate) async fn source_channel_ids_by_source(pool: &SqlitePool, project: ProjectId) -> Result<HashMap<Uuid, Vec<Uuid>>, MetaError>;
pub(crate) async fn set_source_channels(conn: &mut SqliteConnection, source: Uuid, channels: &[Uuid]) -> Result<(), MetaError>;
pub(crate) async fn set_last_notified_at(conn: &mut SqliteConnection, source: Uuid, at: i64) -> Result<(), MetaError>;
pub(crate) async fn due_sources(pool: &SqlitePool, now: i64) -> Result<Vec<NotifySource>, MetaError>;
/// Whether `notify_source` would find at least one event now: the count of step 7.2 on the read pool, so a tick does not take the writer for a source with nothing to send.
pub(crate) async fn has_new_events(pool: &SqlitePool, source: &NotifySource, now: i64) -> Result<bool, MetaError>;
pub(crate) async fn notify_source(conn: &mut SqliteConnection, source: &NotifySource, now: i64) -> Result<Notified, MetaError>;
```
`Digest` and `DigestItem` get a hand-written `Debug` that prints counts only (never titles): `Digest { items: <n>, more: <n> }`, `DigestItem { kind }`.

**Steps:**
1. `issue_scope_ids`: read `SELECT merged_into FROM issues WHERE project_id = ? AND id = ?` (`Option<Option<String>>`); `None` (no row) returns `Ok(None)`. The target is `merged_into` when set, else `issue` (a merge keeps chains one level deep, D-IC-9). Return `Some([target] ++ merged_ids(pool, project, target))` (`meta::issues::merged_ids`, ordered by id, target first).
2. `source_scope_ids`: `SELECT 1 FROM issue_sources WHERE project_id = ? AND id = ?`; none returns `Ok(None)`. Else return `Some(ids)` of:
   ```sql
   SELECT id FROM issues
    WHERE project_id = ?1 AND source_id = ?2 AND state NOT IN ('ignored', 'merged')
   UNION
   SELECT m.id FROM issues m JOIN issues t ON m.merged_into = t.id
    WHERE t.project_id = ?1 AND t.source_id = ?2 AND t.state NOT IN ('ignored', 'merged')
   ORDER BY id
   ```
   (numbered placeholders: bind two values, the project as `?1` and the source as `?2`; the final `ORDER BY id` applies to the whole compound select). An empty list is `Some(vec![])`.
3. `source_channel_ids`: `SELECT channel_id FROM issue_source_channels WHERE source_id = ? ORDER BY channel_id`, parsed to `Uuid`. `source_channel_ids_by_source`: the same for every source of `project` (`JOIN issue_sources s ON s.id = c.source_id WHERE s.project_id = ?`), grouped in a `HashMap` (sources without channels are absent).
4. `set_source_channels`: `DELETE FROM issue_source_channels WHERE source_id = ?`, then one `INSERT` per id. The caller validated the ids (M4.6).
5. `set_last_notified_at`: `UPDATE issue_sources SET last_notified_at = ? WHERE id = ?`.
6. `due_sources`, the selection query (exact):
   ```sql
   SELECT s.id, s.last_notified_at
     FROM issue_sources s
     JOIN projects p ON p.id = s.project_id
     JOIN orgs o ON o.id = p.org_id
    WHERE s.enabled = 1 AND p.state = 'active' AND o.state = 'active'
      AND s.last_notified_at IS NOT NULL
      AND s.last_notified_at <= ?1 - 600000000000
      AND EXISTS (SELECT 1 FROM issue_source_channels c WHERE c.source_id = s.id)
    ORDER BY s.last_notified_at, s.id
   ```
   (`?1` is `now`; a paused source is included, a disabled one is not.) Use the constant `THROTTLE_NS` in Rust by binding `now - THROTTLE_NS` as `?1` instead of the literal; the SQL then reads `s.last_notified_at <= ?1`.
7. `notify_source`, in this order, all on `conn` (the caller runs it inside one `state.meta.write`):
   1. Re-read `last_notified_at` for `source.id`; when it differs from `source.last_notified_at`, or the source is gone, return `Changed`.
   2. Count the events (exact; the same text is a private `const EVENT_COUNT_SQL` that `has_new_events` also uses):
      ```sql
      SELECT COUNT(*) FROM issue_events e JOIN issues i ON i.id = e.issue_id
       WHERE i.source_id = ?1 AND e.kind IN ('created', 'regressed') AND e.at > ?2 AND e.at <= ?3
      ```
      (`?2` = `source.last_notified_at`, `?3` = `now - ISSUE_NOTIFY_SETTLE_NS`: the settle margin). Zero returns `NoEvents` (nothing written, `last_notified_at` unchanged). No filter on the issue's state (Q10). `last_notified_at` advances to the newest collected `at`, never to `now`.
   3. Read the newest 20 (exact, `?3` as above):
      ```sql
      SELECT e.at, e.kind, i.id AS issue_id, i.title
        FROM issue_events e JOIN issues i ON i.id = e.issue_id
       WHERE i.source_id = ?1 AND e.kind IN ('created', 'regressed') AND e.at > ?2 AND e.at <= ?3
       ORDER BY e.at DESC, e.id DESC LIMIT 20
      ```
      `more` is the count minus the rows read. The newest event's `at` is the first row's.
   4. Read the channels with `source_channel_ids`-style SQL on `conn`; none returns `Changed`.
   5. Fence and advance: `UPDATE issue_sources SET last_notified_at = ?1 WHERE id = ?2 AND last_notified_at = ?3` with the newest `at`; zero rows affected returns `Changed`.
   6. Insert `issue_notifications (id, source_id, digest, created_at)` with `Uuid::now_v7()`, the digest as `serde_json::to_string` (a `Digest` always serializes; map the error to `corrupt(...)` anyway) and `created_at = now`.
   7. Insert one delivery per channel (exact):
      ```sql
      INSERT INTO notification_deliveries
        (id, event_id, issue_notification_id, channel_id, status, attempts, next_attempt_at, last_error, created_at, updated_at)
      VALUES (?, NULL, ?, ?, 'pending', 0, ?, NULL, ?, ?)
      ```
      with `next_attempt_at = now`, `created_at = updated_at = now`.
   8. Return `Sent { … }`.

**Tests** (in the file's `#[cfg(test)]` module; use `TestApp`/`seed` and the helpers below; each event's `at` is set explicitly):
- `issue_scope_includes_the_merged_set`: an issue with two merged issues returns `[issue, a, b]`.
- `issue_scope_of_a_merged_issue_is_its_targets`: asking for a merged issue returns the target and every issue merged into the target (merge chains are one level deep).
- `issue_scope_of_a_missing_issue_is_none`, `issue_scope_is_project_scoped` (an issue of another project is `None`).
- `source_scope_skips_ignored_and_merged_issues`: open and resolved issues are in; an ignored issue and the issues merged into it are out; a merged issue whose target is open is in.
- `source_scope_of_a_source_without_issues_is_empty_not_none`, `source_scope_of_a_missing_source_is_none`.
- `source_channels_round_trip_sorted`; `set_source_channels_replaces`; `deleting_a_channel_removes_it_from_a_source` (delete the `notification_channels` row by SQL; the link is gone); `deleting_a_source_removes_its_links_and_notifications`.
- `due_sources_requires_enabled_channels_and_the_throttle`: not due at `last_notified_at + THROTTLE_NS - 1`, due at `last_notified_at + THROTTLE_NS`; not due when disabled; not due without channels; not due when `last_notified_at` is `NULL`; due when paused (`paused_reason` set); not due when the project or org is not `active`.
- `notify_source_writes_one_notification_per_channel`: two channels give two pending deliveries with `event_id IS NULL`, one `issue_notifications` row, and `last_notified_at` equal to the newest event's `at`.
- `notify_source_digest_is_newest_first_and_capped`: 25 events give 20 items (the 20 newest, in `at` descending order) and `more = 5`.
- `notify_source_counts_only_created_and_regressed`: `state_changed`, `renamed` and `merged` events are ignored; with only those, the result is `NoEvents`.
- `notify_source_ignores_events_at_or_before_the_cutoff`: an event with `at == last_notified_at` is not sent; one with `at == now - ISSUE_NOTIFY_SETTLE_NS` is.
- `notify_source_waits_for_events_inside_the_settle_margin`: an event with `at == now - ISSUE_NOTIFY_SETTLE_NS + 1` is not collected (`NoEvents`, `last_notified_at` unchanged); a later call with `now` 1 s later sends it. With two events, one inside and one outside the margin, only the outside one is sent and `last_notified_at` equals its `at` (the inside one is sent by the next tick).
- `has_new_events_matches_notify_source`: true with an event outside the margin, false with none or only one inside it or at the cut-off.
- `notify_source_with_no_events_changes_nothing`: `NoEvents`, `last_notified_at` unchanged, no rows.
- `notify_source_returns_changed_when_the_cutoff_moved`: change `last_notified_at` after building the `NotifySource`; nothing is written.
- `the_delivery_check_refuses_both_or_neither`: inserting a delivery with both `event_id` and `issue_notification_id` set, or with neither, fails the CHECK.
- `the_rebuilt_deliveries_table_keeps_its_indexes`: `PRAGMA index_list(notification_deliveries)` names the four indexes of M4.1.
- `digest_debug_hides_titles`: `format!("{:?}", digest)` does not contain a title.

**Test helpers** (`test_support/issues.rs`, `#![allow(dead_code)]` with a comment that the Closing removes it because later phases use the rest; this phase creates the whole file; later phases use it and never copy it). Imports: `crate::meta::issues::{self as store, EventKind, EventRow, IssueRow, IssueState, SourceKind, SourceRow}`, `crate::scores::{self as score_writes, Writer}`, `crate::api::scores::ScoreCreate`, `cetra_core::ids::TraceId`, `cetra_core::score::ScoreValue`, `super::{Seeded, TestApp}`, as the `routes/issues.rs` tests import them:
```rust
/// A source with every optional field `None` or empty, `created_at = updated_at = now`, no creator.
pub(crate) fn source_row(project: ProjectId, name: &str, kind: SourceKind, enabled: bool, now: i64) -> SourceRow
pub(crate) async fn put_source(app: &TestApp, row: SourceRow)
/// The body of `issue_row` in the `routes/issues.rs` tests: `first_seen = last_seen = created_at = updated_at = seen`, `regressed` false, no fingerprint, description, merge target or centroid, `member_count` 0.
pub(crate) fn issue_row(project: ProjectId, source: Uuid, id: Uuid, title: &str, state: IssueState, seen: i64) -> IssueRow
pub(crate) async fn put_issue(app: &TestApp, row: IssueRow)
/// One event with a fresh `Uuid::now_v7()` id, no actor, no from/to value, at `at`.
pub(crate) async fn put_event(app: &TestApp, issue: Uuid, kind: EventKind, at: i64)
/// A notification channel row (kind slack, sealed config that is never opened) for tests that never deliver; returns its id. Tests that deliver create channels through the channels route.
pub(crate) async fn put_channel(app: &TestApp, project: ProjectId, name: &str) -> Uuid
/// One occurrence score of `issue` on `trace` (32 hex), `created_at = at` (absolute nanoseconds), written as the issue writer does (D-IC-18): the body of `occurrence` in the `routes/issues.rs` tests with `at` given instead of `hours_ago`.
pub(crate) async fn occurrence(app: &TestApp, seeded: &Seeded, source: Uuid, issue: Uuid, trace: &str, at: i64, user: Option<&str>)
```
`put_channel` inserts with `sqlx::query` through `app.state.meta.write` into `notification_channels (id, project_id, kind, name, config_enc, target_hint, created_at, updated_at)` with `config_enc = vec![0]`, `target_hint = "test"`. The `routes/issues.rs` tests keep their own private `occurrence` (hours-based); M4.10a replaces their private `issue_row` and `put_issue` by these.

**Done when:** `rustfmt --edition 2024` on the four files succeeds; by hand, every SQL statement above appears with the same columns.

**Commit:** `Add the issue link store`

---

## M4.4 K condition: API, validation and the rule route

**Goal:** `issue_occurrences` exists in the API type, is validated, and is checked against the project when a rule is saved.

**Edit:**
- `crates/server/src/api/alerts.rs`;
- `crates/server/src/alerts/condition.rs`;
- `crates/server/src/alerts/message.rs` (`value_text` and the `sample_notice` literal only);
- `crates/server/src/routes/alert_rules.rs`;
- `crates/server/src/routes/channels.rs` (the `AlertCondition` literal only);
- `crates/server/src/meta/alerts.rs` (the test literals only);
- `crates/server/src/alerts/deliver/tests.rs` (the `rule_condition` literal only);
- `crates/server/src/alerts/evaluate.rs` (the one `measure(condition)` call in `measure_rule` and the `AlertCondition` literals of its tests; nothing else).

**Contract** (`api/alerts.rs`):
```rust
pub enum AlertMetric {
    // …ErrorRate, Cost, LatencyP95, Score…
    /// Occurrences of one issue or of one source's issues in the window (D-IC-20).
    IssueOccurrences,
}
// as_str: Self::IssueOccurrences => "issue_occurrences"
// the as_str doc comment lists the five spellings.

pub struct AlertCondition {
    // …existing fields in the existing order…
    /// `issue_occurrences` only: count one issue and the issues merged into it. Exactly one of
    /// `issue_id` and `source_id` is set for that metric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<Uuid>,
    /// `issue_occurrences` only: count every issue of the source that is not ignored or merged,
    /// and the issues merged into them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<Uuid>,
}
```
The doc comments of `filter`, `min_count`, `target`, `score_name`, `aggregation` get "not for `issue_occurrences`" where a metric list is given. The doc of `AlertMetric` ("What a rule measures (D-K-2)") stays.

**Steps:**
1. `api/alerts.rs`: add the variant, its `as_str` arm and the two fields.
2. `alerts/condition.rs`, `normalize_condition` (keep the six steps and their order; error text in quotes is exact):
   - step 1 (filter): after trimming, when `condition.metric == AlertMetric::IssueOccurrences` and the filter is `Some`, push `error("filter", "does not apply to issue_occurrences")` and clear it.
   - step 2 (threshold): in the `match condition.metric`, `AlertMetric::IssueOccurrences => (threshold < 0.0).then_some("must be at least 0")` (the non-finite case is already handled above it).
   - step 3 (`min_count`): the `Cost` arm becomes `AlertMetric::Cost | AlertMetric::IssueOccurrences`, and its message `&format!("does not apply to {}", condition.metric.as_str())` (the helper takes `&str`) (the text for `cost` stays `does not apply to cost`); `min_count` is set to `None`.
   - steps 4 to 6 need no code: `target`, `score_name` and `aggregation` already push "applies to … only" when set for another metric.
   - new step 7 (window): for `IssueOccurrences`, `AlertWindow::M1` or `AlertWindow::M5` → `error("window", "issue occurrences need a window of 15m or more")` (D-IC-20: an occurrence is written after its trace completes, so a short window misses it).
   - new step 8 (scope), after step 7: for `IssueOccurrences`: both set → `error("source_id", "cannot be set with issue_id")`; neither → `error("issue_id", "one of issue_id and source_id is required")`. For every other metric: `issue_id` set → `error("issue_id", "applies to issue_occurrences only")`; `source_id` set → `error("source_id", "applies to issue_occurrences only")`.
   - `measure(condition: &AlertCondition) -> Option<AlertMeasure>`: every existing arm wrapped in `Some(...)`; `AlertMetric::IssueOccurrences => None` (the evaluator builds its own measure from the resolved names, M4.5). Update the doc comment.
   - `alerts/evaluate.rs`, `measure_rule`: replace `measure: measure(condition),` by a `let Some(measure) = measure(condition) else { return Err("query failed") };` placed before the request (no `unwrap`; M4.5 adds the issue branch above it, so the `else` is never taken). Fix the `AlertCondition` literals of the file's tests (`error_rate()` helper and the `cost` literal) with `issue_id: None, source_id: None`.
3. `alerts/message.rs`: `value_text`: `AlertMetric::IssueOccurrences => format!("{value:.0}")`; update the doc comment (`12` for `issue_occurrences`). `sample_notice`: nothing to add (it has no `AlertCondition`). `Notice`-building code elsewhere is unchanged.
4. `routes/alert_rules.rs`: `checked_condition`, after `normalize_condition` and the filter check, add the existence check, building `ApiError::BadRequest { detail: None, errors: vec![FieldError { field, message, position: None }] }` (reuse the error shape the function already returns):
   - `issue_id` set: `crate::meta::issues::get_issue(state.meta.reader(), project, id)`; `None` → field `condition.issue_id`, message `"no such issue in this project"`;
   - `source_id` set: `crate::meta::issues::get_source(state.meta.reader(), project, id)`; `None` → field `condition.source_id`, message `"no such issue source in this project"`.
   Change `async fn checked_channels` to `pub(crate) async fn checked_channels` (M4.6 reuses it) and `MAX_RULE_CHANNELS` is already `pub(crate)` in `meta/alerts.rs`. Add `"condition.issue_id"` and `"condition.source_id"` nowhere (field errors pass through).
5. Fix every struct literal of `AlertCondition` with `issue_id: None, source_id: None`: `routes/channels.rs` (one), `meta/alerts.rs` tests (one, the `condition(threshold)` helper), `alerts/deliver/tests.rs` (`rule_condition`), `alerts/evaluate.rs` tests (two), `alerts/condition.rs` tests (the `condition(metric)` helper). Fix every other literal that `grep -rn "AlertCondition {" crates/server/src` finds. The `condition(metric)` helper of `condition.rs` has window `5m`: tests that build an `IssueOccurrences` condition from it set `window: AlertWindow::M15`.
6. Q5: the alert message code is unchanged: an `issue_occurrences` rule has no filter, so its "View traces" link is the trace list over the window.

**Tests** (in `alerts/condition.rs` and `routes/alert_rules.rs` test modules):
- `issue_occurrences_refuses_short_windows`: `1m` and `5m` give `condition.window` "issue occurrences need a window of 15m or more"; `15m`, `1h`, `6h`, `24h` pass; `error_rate` over `1m` still passes.
- `issue_occurrences_threshold_is_checked`: `-1` is refused with `condition.threshold` "must be at least 0"; `0`, `3` and `2.5` pass; `NaN` is refused as for other metrics.
- `issue_occurrences_refuses_fields_that_do_not_apply`: `filter`, `min_count`, `target`, `score_name` and `aggregation` each give their own field error; all of them together are collected in field order.
- `issue_occurrences_needs_exactly_one_scope`: neither → `condition.issue_id` error; both → `condition.source_id` error; one passes and is kept.
- `other_metrics_refuse_issue_and_source_ids`: `error_rate` with `issue_id` → `condition.issue_id` "applies to issue_occurrences only"; likewise `source_id`.
- `issue_occurrences_json_round_trips`: serialize and parse back; `issue_id` serialized as a UUID string; an unknown field is still refused; an invalid UUID string is a parse error.
- `measure_follows_the_condition` (existing) updated for `Option` (every old arm is `Some(...)`); `issue_occurrences_has_no_storage_measure`: `measure` returns `None`.
- `value_text_formats` (existing) gets `issue_occurrences`: `value_text(IssueOccurrences, Some(12.0)) == "12"`, `Some(0.0) == "0"`, `None == "no data"`.
- Route tests (`TestApp`): `create_issue_rule_with_a_known_issue` (201, condition echoes `issue_id`, `min_count` absent); `create_rule_with_an_unknown_issue_is_400` (`condition.issue_id`, "no such issue in this project"); `create_rule_with_an_issue_of_another_project_is_400`; `create_rule_with_an_unknown_source_is_400` (`condition.source_id`); `create_source_rule_with_a_known_source`; `update_to_an_unknown_issue_is_400`; `issue_rule_counts_toward_the_rule_limit` (with the plan limit helper the existing quota tests use).

**Done when:** `rustfmt --edition 2024` on every edited `.rs` file succeeds; `grep -rn "AlertCondition {" crates/server/src` shows only literals with the two new fields; `grep -rn "measure(condition)" crates/server/src` shows only the `let Some(measure)` form.

**Commit:** `Add the issue occurrences alert condition`

---

## M4.5 K evaluation of `issue_occurrences`

**Goal:** a rule on `issue_occurrences` is measured as D-IC-20 says.

**Edit:** `crates/server/src/alerts/evaluate.rs` (tests included, with its own `AlertCondition` literals).

**Steps:**
1. (M4.4 already replaced the `measure: measure(condition)` field by a `let Some(measure) = measure(condition) else { return Err("query failed") };` and fixed this file's test literals. Keep both.) Refactor `measure_rule` so both paths share what is common. Add two private helpers in the file:
   ```rust
   async fn open_ctx(state: &AppState, org: OrgId, project: ProjectId) -> Result<QueryContext, &'static str>
   // the open_query_on(...) call and its ApiError mapping ("storage is busy" / "query failed") as today
   fn run_reason(error: RunError) -> &'static str
   // the RunError mapping as today: Timeout "query timed out", TooLarge "query needed more memory or temporary disk space than allowed", Invalid "the filter is no longer valid", _ "query failed"
   ```
   `measure_rule` keeps its signature and behaviour for the four old metrics (use the helpers). Right after building `window`, before `open_ctx` and before the `measure(condition)` call: `if condition.metric == AlertMetric::IssueOccurrences { return measure_issue_rule(state, org, project, condition, window).await; }`. The `else { return Err("query failed") }` of the `measure` call is then never taken for a stored rule.
2. New function:
   ```rust
   /// Counts the occurrence scores of the rule's issue or source over `window` (D-IC-20): the
   /// names are resolved from `meta.db` now, split in chunks of `MAX_COUNT_NAMES`, one
   /// `alert_value` each, and the counts are summed.
   async fn measure_issue_rule(
       state: &AppState,
       org: OrgId,
       project: ProjectId,
       condition: &AlertCondition,
       window: TimeWindow,
   ) -> Result<AlertValue, &'static str>
   ```
   Body, in order:
   - `let ids = match (condition.issue_id, condition.source_id)`: `(Some(issue), None)` → `issue_scope_ids(state.meta.reader(), project, issue).await.map_err(|_| "query failed")?.ok_or("issue not found")?`; `(None, Some(source))` → `source_scope_ids(...)` with `.ok_or("issue source not found")?`; anything else → `Err("query failed")` (the API refuses it; a stored row is never both or neither).
   - `let names: Vec<String> = ids.iter().map(|id| format!("issue:{id}")).collect();` (the name format of D-IC-11; `routes/issues.rs::score_name` is the same string).
   - empty `names` → `Ok(AlertValue { count: 0, value: Some(0.0) })` without opening storage.
   - `let ctx = open_ctx(state, org, project).await?;` then for each `chunk` of `names.chunks(MAX_COUNT_NAMES)`: `Box::pin(alert_value(&ctx, AlertValueRequest { window, filter: None, measure: AlertMeasure::ScoreCount { names: chunk.to_vec() }, timeout: QUERY_TIMEOUT })).await.map_err(run_reason)?`, adding `count` (saturating) to a total.
   - return `AlertValue { count: total, value: Some(total as f64) }`.
   The fixed reasons `issue not found` and `issue source not found` flow into `Evaluation::Failed` as any failed read does (K's existing path: the rule stays in its state, `last_error` set).
3. `reading` needs no change: `min_count` is `None` for this metric, so a value is `Breaching` or `Clear`, never `Insufficient`. Add a comment on `reading`.
4. `use` additions: `cetra_storage::MAX_COUNT_NAMES`, `cetra_storage::QueryContext` (if not imported), `crate::meta::issue_links::{issue_scope_ids, source_scope_ids}`.

**Tests** (in the file's test module; build sources, issues and occurrence scores with the helpers of `crate::test_support::issues` that M4.3 created: `source_row`/`put_source`, `issue_row`/`put_issue`, `occurrence(app, seeded, source, issue, trace, at, user)`; `measure_rule` is called directly with explicit `from`/`to`, so the API's window rule does not apply to it):
- `issue_rule_counts_occurrences_in_the_window`: 3 scores in the window and 2 outside give `count 3`, `value Some(3.0)`.
- `issue_rule_counts_merged_issues_with_their_target`: occurrences of a merged issue count on its target.
- `issue_rule_on_a_merged_issue_follows_its_target`: a rule whose `issue_id` was later merged counts the target's whole group (D-IC-20).
- `issue_rule_counts_an_ignored_issue`: an issue-scoped rule on an ignored issue still counts.
- `source_rule_skips_ignored_and_merged_issues`: ignored issues (and what is merged into them) do not count; open, resolved issues and the issues merged into them do.
- `source_rule_with_no_issues_is_zero`: value `0.0`, `count 0`, no storage opened.
- `zero_occurrences_is_a_value_and_can_breach`: comparator `lt` threshold `1` over an empty window breaches (zero is not "no data").
- `names_over_the_cap_are_chunked`: a source with 1 001 issues (insert rows by SQL in one transaction; write one occurrence score for the first and one for the last) sums to 2 across two reads.
- `missing_issue_fails_with_a_fixed_reason`: after deleting the issue's source by SQL, `measure_rule` returns `Err("issue not found")`; for a source rule `Err("issue source not found")`; `evaluate_once` records `last_error` with that reason and keeps the state.
- `deleting_a_source_leaves_its_rules`: `DELETE …/issue-sources/{id}` leaves the alert rule in place (it then fails with the fixed reason).
- `issue_rule_fires_and_resolves`: with the manual clock, a rule `gt 0` over `15m` (stored through the meta store; the API refuses shorter windows) goes `fired` when an occurrence is written and `resolved` after the window passes (`Clock::advance`), using `evaluate_once(&app.state, now)` directly.
- `old_metrics_are_unchanged`: the existing tests of this file still describe the four metrics (no edit needed; do not delete any).

**Done when:** `rustfmt --edition 2024 crates/server/src/alerts/evaluate.rs` succeeds; `measure_rule` has no copy of the `open_query_on` call; the `IssueOccurrences` early return precedes the `measure(condition)` call.

**Commit:** `Evaluate issue occurrence alert rules`

---

## M4.6 Source channels API

**Goal:** sources carry `channel_ids`; enabling a source starts its notification clock (D-IC-21).

**Edit:** `crates/server/src/api/issues.rs`; `crates/server/src/routes/issues.rs`.

**Contract** (`api/issues.rs`; place the new field after the existing `enabled` field of each type; M3's fields stay):
```rust
// CreateIssueSourceRequest
/// Channels notified of new and regressed issues, at most 10 channels of this project. Default none.
#[serde(default)]
pub channel_ids: Vec<Uuid>,

// UpdateIssueSourceRequest
/// New channels, at most 10 of this project. `[]` removes every channel. Absent keeps them.
#[serde(default, deserialize_with = "present")]
pub channel_ids: Option<Vec<Uuid>>,

// IssueSource (response)
/// Channels notified of new and regressed issues. Ordered by id.
pub channel_ids: Vec<Uuid>,
```
Fix every other constructor of `IssueSource`, `CreateIssueSourceRequest` and `UpdateIssueSourceRequest` in the crate (`grep -rn "IssueSource {\|CreateIssueSourceRequest {\|UpdateIssueSourceRequest {" crates/server/src`).

**Steps:**
1. `routes/issues.rs`, `source_json(row, open_issues)` takes the channel ids as a third parameter `channel_ids: Vec<Uuid>`. Callers:
   - `list_issue_sources`: one read `issue_links::source_channel_ids_by_source(state.meta.reader(), project)`; each row gets `map.get(&row.id).cloned().unwrap_or_default()`;
   - `get_issue_source` and `update_issue_source`: `issue_links::source_channel_ids(state.meta.reader(), id)`;
   - `create_issue_source`: the validated channel ids.
2. Validation: `let channels = crate::routes::alert_rules::checked_channels(&state, project, &body.channel_ids).await?` (create), and for update only when `body.channel_ids` is `Some`. It returns the sorted, unique, project-owned ids, or `400` on `channel_ids` ("at most 10 channels", "duplicate channel", "unknown channel"). Keep the existing order and call it after `validate`.
3. Create: `insert_checked(conn, org, row, limit, channels, now)`: after `insert_source`, `issue_links::set_source_channels(conn, row.id, &channels)`; and when `row.enabled`, `issue_links::set_last_notified_at(conn, row.id, now)` (`now` is the `state.clock.now_ns()` the handler already read; it equals `row.created_at`).
4. Update: `update_checked(conn, project, org, stored, row, limit, channels: Option<Vec<Uuid>>, now)`: after `update_source`, `if let Some(channels) = channels { set_source_channels(...) }`; and when `!stored.enabled && row.enabled`, `set_last_notified_at(conn, row.id, now)`. A PATCH that only changes channels does not touch `last_notified_at`. The response reads the channel ids after the write.
5. Delete needs nothing (foreign keys cascade). Audit logs keep ids only.
6. Permission is unchanged (`ManageEvaluation` on the source routes).

**Tests** (route tests in the existing module, `TestApp`; a Slack channel is created with the channels route as `routes/alert_rules.rs` tests do):
- `create_source_with_channels`: 201, response `channel_ids` sorted; `GET` returns them; the list page returns them per source.
- `create_source_without_channels_returns_an_empty_list`.
- `channel_ids_are_validated`: 11 ids → 400 `channel_ids` "at most 10 channels"; a duplicate → "duplicate channel"; a channel of another project → "unknown channel".
- `patch_replaces_and_clears_channels`: `[a]` → `[b]` → `[]`; absent keeps; `null` is refused (400, as other optional fields).
- `creating_an_enabled_source_sets_last_notified_at`: the column equals the clock's `now` (read by SQL).
- `creating_a_disabled_source_leaves_last_notified_at_null`; `enabling_a_disabled_source_sets_it_to_now` (clock advanced first); `disabling_and_re_enabling_resets_it` (again later `now`).
- `a_patch_that_keeps_the_source_enabled_leaves_last_notified_at` (clock advanced; value unchanged).
- `deleting_a_channel_removes_it_from_a_source` (route `DELETE …/channels/{id}`, then `GET` the source: `channel_ids` empty).
- `a_viewer_cannot_set_channels` (a `ProjectPerm::ManageEvaluation` refusal is the existing behaviour: one assertion with a viewer cookie gets 403).

**Done when:** `rustfmt --edition 2024` on the edited files succeeds; no constructor of the three types is left without `channel_ids`.

**Commit:** `Add notification channels to issue sources`

---

## M4.7 Issue messages

**Goal:** the Slack, webhook and email messages of one issue notification (D-IC-21).

**Edit:** `crates/server/src/alerts/message.rs`; `crates/server/src/email/mod.rs`; new `crates/server/src/email/templates/issue_notification.html`; new `crates/server/src/email/templates/issue_notification.txt`.

**Contract** (`alerts/message.rs`):
```rust
/// Everything an issue notification shows. Never log it: it holds titles and names.
#[derive(Clone, PartialEq)]
pub(crate) struct IssuesNotice {
    pub(crate) notification_id: Uuid,
    pub(crate) source_id: Uuid,
    pub(crate) source_name: String,
    pub(crate) project_id: ProjectId,
    pub(crate) project_name: String,
    pub(crate) items: Vec<DigestItem>,   // crate::meta::issue_links::DigestItem
    pub(crate) more: u64,
    /// When the notification was recorded.
    pub(crate) occurred_at: i64,
    /// `<public_url>/<org>/<project>/issues?source=<source id>`.
    pub(crate) issues_url: String,
    /// The issue page URL of each item, in the order of `items`.
    pub(crate) issue_urls: Vec<String>,
}
// Debug: hand-written, counts only (items, more), never titles or names.

/// `<public_url>/<org_slug>/<project_slug>/issues/<issue_id>`.
pub(crate) fn issue_url(public_url: &PublicUrl, org_slug: &str, project_slug: &str, issue_id: Uuid) -> String
/// `<public_url>/<org_slug>/<project_slug>/issues?source=<source_id>`.
pub(crate) fn issues_url(public_url: &PublicUrl, org_slug: &str, project_slug: &str, source_id: Uuid) -> String
pub(crate) fn issues_slack_body(notice: &IssuesNotice) -> serde_json::Value
pub(crate) fn issues_webhook_body(notice: &IssuesNotice) -> Vec<u8>
pub(crate) fn issues_email(notice: &IssuesNotice) -> Email
```

**Webhook body** (exact JSON shape; times are RFC 3339 through `time_serde`, as the alert payload's):
```json
{
  "type": "issues.detected",
  "notification_id": "<uuid>",
  "occurred_at": "<rfc3339>",
  "source_id": "<uuid>",
  "source_name": "<text>",
  "project": {"id": "<uuid>", "name": "<text>"},
  "items": [{"issue_id": "<uuid>", "kind": "created", "title": "<full title>"}],
  "more": 0,
  "issues_url": "<url>"
}
```
Implement as a `#[derive(Serialize)] struct IssuesPayload<'a>` (like `WebhookPayload`, reusing `WebhookProject`). `type` `issues.detected`; `kind` values `created`/`regressed` (the `DigestKind` serde names). Q1 applies to `type`, `notification_id` and `occurred_at`.

**Slack body** (exact structure; `slack_escape` the `mrkdwn` texts and the top-level `text` only, never the `plain_text` header, as the alert body does; titles cut to 100 characters before escaping, Q7):
```json
{
  "text": "<N> new or regressed issues in <source>",
  "blocks": [
    {"type": "header", "text": {"type": "plain_text", "text": "<N> new or regressed issues in <source>"}},
    {"type": "section", "text": {"type": "mrkdwn", "text": "• *new* <<issue url>|<title>>"}},
    …one section per item, `*new*` for created and `*regressed*` for regressed…,
    {"type": "context", "elements": [{"type": "mrkdwn", "text": "and <more> more"}]},
    {"type": "actions", "elements": [{"type": "button", "text": {"type": "plain_text", "text": "View issues"}, "url": "<issues url>"}]}
  ]
}
```
`<N>` is `items.len() + more`; the noun comes from `pub(crate) fn issues_noun(total: u64) -> &'static str` (in `email/mod.rs`, imported by `message.rs`): `"issue"` for 1, else `"issues"` (Q9), so the header and the top-level `text` read `1 new or regressed issue in <source>` or `N new or regressed issues in <source>`. The `context` block appears only when `more > 0`. The header text is cut to 150 characters (the source name is the variable part; Slack limits headers to 150). A Slack link is `<url|title>`: escape `|`, `<`, `>`, `&` in the title with `slack_escape` plus replacing `|` by `¦`.

**Email:** a new variant and line type in `email/mod.rs`:
```rust
/// One line of an issue notification email.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct IssueLine { pub(crate) kind: &'static str, pub(crate) title: String, pub(crate) url: String }

// in enum Email:
/// New or regressed issues of a source (D-IC-21).
Issues {
    source_name: String,
    project_name: String,
    /// `items.len() + more`.
    total: u64,
    /// At most 20; titles cut to 100 characters.
    items: Vec<IssueLine>,
    more: u64,
    issues_url: String,
},
```
`Email` derives no `Debug` (`email/mod.rs`), and `IssueLine` derives `Debug, Clone, PartialEq, Serialize` as shown, so no title can reach a log through a derive. Add `pub(crate) fn issues_noun(total: u64) -> &'static str` here too (`"issue"` for 1, else `"issues"`).
- Subject (`subject()`): `format!("[{PRODUCT}] {total} new or regressed {}: {source_name}", issues_noun(*total))`.
- `template_name`: `Email::Issues { .. } => "issue_notification"`.
- Register `issue_notification.txt` and `issue_notification.html` in the template list next to `alert.*` (`include_str!`).
- Render arm: `("issue_notification.txt", "issue_notification.html", context! { source_name => source_name, project_name => project_name, total => *total, noun => issues_noun(*total), items => items_values, more => *more, issues_url => issues_url })` where `items_values` is `Vec<minijinja::Value>` built with `minijinja::Value::from_serialize(line)`.

**Templates:**
- `issue_notification.txt`:
  ```
  {{ total }} new or regressed {{ noun }} in {{ source_name }} ({{ project_name }})
  {% for item in items %}
  - {{ item.kind }}: {{ item.title }}
    {{ item.url }}{% endfor %}{% if more > 0 %}

  and {{ more }} more{% endif %}

  View the issues:
  {{ issues_url }}
  ```
- `issue_notification.html`: the layout of `alert.html` (same table skeleton and inline styles), title `{{ total }} new or regressed {{ noun }}: {{ source_name }}`; a bold first paragraph `{{ total }} new or regressed {{ noun }} in {{ source_name }}`; a paragraph `{{ project_name }}`; a `<ul>` of `<li>{{ item.kind }}: <a href="{{ item.url }}">{{ item.title }}</a></li>` per item; `{% if more > 0 %}<p>and {{ more }} more</p>{% endif %}`; a last paragraph `<a href="{{ issues_url }}">View the issues</a>`. `.html` templates auto-escape.

**Steps:**
1. Add the types, URL helpers and three body functions to `message.rs`; the item kind word for Slack/email is `new` for `Created` and `regressed` for `Regressed` (`fn kind_word(DigestKind) -> &'static str`).
2. Add the `Email::Issues` variant, `IssueLine`, subject, template name, template registration and render arm in `email/mod.rs`; check the `Email` enum's other `match`es (`grep -n "match email\|Email::Alert" crates/server/src/email/mod.rs`) and add the arm everywhere.
3. Write the two templates.

**Tests** (in `message.rs` and `email/mod.rs` test modules; a `sample_issues_notice(items, more)` helper in `message.rs` tests):
- `issues_webhook_body_has_the_documented_shape`: parse the bytes; exact keys, `type == "issues.detected"`, kinds, `more`, `issues_url`; the item has no `url` key.
- `issues_webhook_body_keeps_the_whole_title` (a 300-character title is not cut).
- `issues_slack_body_singular`: one item and `more 0`: header text and top-level `text` are `"1 new or regressed issue in Errors"`.
- `issues_slack_body_header_and_lines`: header text `"3 new or regressed issues in Errors"` for 3 items and `more 0`; one section per item with the `*new*`/`*regressed*` word and a link to the issue URL; no `context` block.
- `issues_slack_body_with_more`: 20 items + `more 5` → header says 25, 20 item sections, a `context` block `and 5 more`, an `actions` block whose button is `View issues` with the issues URL.
- `issues_slack_body_cuts_and_escapes_titles`: a title with `<`, `>`, `&`, `|` and 300 characters is cut to 100 and escaped; the header is at most 150 characters for a 200-character source name.
- `issue_urls_are_built_from_the_public_url`: `issue_url` and `issues_url` with a path-prefixed public URL.
- `issues_email_subject_and_body`: subject `"[<PRODUCT>] 3 new or regressed issues: Errors"`; for one item the subject is `"[<PRODUCT>] 1 new or regressed issue: Errors"` and both the text and HTML bodies say `1 new or regressed issue in Errors` (never `issues`); text and HTML render without error for 0 and 20 items, with and without `more`; the HTML escapes a title containing `<script>`.
- `issues_noun_is_singular_for_one`: `issues_noun(0) == "issues"`, `(1) == "issue"`, `(2) == "issues"`.
- `notice_debug_hides_titles` (`format!("{:?}", notice)` contains no title or source name).

**Done when:** `rustfmt --edition 2024` on the two `.rs` files succeeds; by hand, every `match` on `Email` has the new arm.

**Commit:** `Add issue notification messages`

---

## M4.8 Issue deliveries

**Goal:** the delivery loop reads and sends issue notifications as it does rule events.

**Edit:** `crates/server/src/meta/alerts.rs`; `crates/server/src/alerts/deliver.rs`; `crates/server/src/alerts/deliver/tests.rs`; `crates/server/src/routes/channels.rs` (the import and the one call to `send`, nothing else).

**Contract** (`meta/alerts.rs`): `DueDelivery` carries a body enum instead of `event`:
```rust
pub(crate) struct DueDelivery {
    pub(crate) id: Uuid,
    pub(crate) attempts: i64,
    pub(crate) channel_id: Uuid,
    pub(crate) channel_kind: ChannelKind,
    pub(crate) config_enc: Vec<u8>,
    /// What is delivered: a rule event or an issue notification.
    pub(crate) body: DueBody,
    pub(crate) project_id: ProjectId,
    pub(crate) project_name: String,
    pub(crate) project_slug: String,
    pub(crate) org_id: OrgId,
    pub(crate) org_slug: String,
}

#[derive(Clone, PartialEq)]
pub(crate) enum DueBody {
    Rule(EventRow),
    Issues(DueIssues),
}

#[derive(Clone, PartialEq)]
pub(crate) struct DueIssues {
    pub(crate) notification_id: Uuid,
    pub(crate) source_id: Uuid,
    pub(crate) source_name: String,
    pub(crate) digest: crate::meta::issue_links::Digest,
    pub(crate) created_at: i64,
}
```
The existing hand-written `Debug` of `DueDelivery` stays (ids, attempts, channel only); `DueBody`/`DueIssues` get none (not needed) or a counts-only `Debug`.

**`due_deliveries`** keeps its signature `(pool, now, limit) -> Result<Vec<DueDelivery>, MetaError>`. It runs two queries and merges (the top `limit` of the union equals the top `limit` of the two top-`limit` lists, so the merge is exact):

Rule deliveries (the existing query, with `d.next_attempt_at` added to the select and `e` joined as today):
```sql
SELECT d.id, d.attempts, d.next_attempt_at, d.channel_id, c.kind AS channel_kind, c.config_enc,
       e.id AS event_id, e.rule_id, e.kind AS event_kind, e.value, e.count, e.threshold,
       e.window_from, e.window_to, e.details, e.created_at AS event_created_at,
       p.id AS project_id, p.name AS project_name, p.slug AS project_slug, o.id AS org_id, o.slug AS org_slug
  FROM notification_deliveries d
  JOIN notification_channels c ON c.id = d.channel_id
  JOIN alert_events e ON e.id = d.event_id
  JOIN alert_rules r ON r.id = e.rule_id
  JOIN projects p ON p.id = r.project_id
  JOIN orgs o ON o.id = p.org_id
 WHERE d.status = 'pending' AND d.next_attempt_at <= ? AND p.state = 'active' AND o.state = 'active'
 ORDER BY d.next_attempt_at, d.id
 LIMIT ?
```
Issue deliveries:
```sql
SELECT d.id, d.attempts, d.next_attempt_at, d.channel_id, c.kind AS channel_kind, c.config_enc,
       n.id AS notification_id, n.source_id, s.name AS source_name, n.digest,
       n.created_at AS notification_created_at,
       p.id AS project_id, p.name AS project_name, p.slug AS project_slug, o.id AS org_id, o.slug AS org_slug
  FROM notification_deliveries d
  JOIN notification_channels c ON c.id = d.channel_id
  JOIN issue_notifications n ON n.id = d.issue_notification_id
  JOIN issue_sources s ON s.id = n.source_id
  JOIN projects p ON p.id = s.project_id
  JOIN orgs o ON o.id = p.org_id
 WHERE d.status = 'pending' AND d.next_attempt_at <= ? AND p.state = 'active' AND o.state = 'active'
 ORDER BY d.next_attempt_at, d.id
 LIMIT ?
```
Merge: collect `(next_attempt_at, id string, DueDelivery)` triples, sort by `(next_attempt_at, id)`, truncate to `limit`. (`next_attempt_at` is non-null for pending rows; treat a null as `i64::MAX`.) The digest is parsed with `serde_json::from_str::<Digest>`; a failure is `corrupt(DELIVERIES, "digest")`.

**`prune`** gains the issue notifications: signature `prune(conn, events_before, deliveries_before) -> Result<(u64, u64, u64), MetaError>` returning `(events, notifications, deliveries)`; the new statement `DELETE FROM issue_notifications WHERE created_at < ?` (bound to `events_before`: 90 days, D-IC-21) runs after the events delete and before the deliveries statement. Update `prune_once` in `deliver.rs` (`(events, notifications, deliveries)`, the log line adds `notifications`, the return type becomes `(u64, u64, u64)`), and every caller and test destructuring (`grep -rn "prune_once\|prune(conn" crates/server/src/alerts crates/server/src/meta/alerts.rs`).

**`alerts/deliver.rs`:**
1. A borrowed enum (`pub(crate)`, because `routes/channels.rs` builds it):
   ```rust
   pub(crate) enum Outgoing<'a> { Alert(&'a Notice), Issues(&'a IssuesNotice) }
   ```
2. `send_one` is renamed and takes the enum: `pub(crate) async fn send(state: &AppState, org_id: OrgId, config: &ChannelConfig, outgoing: Outgoing<'_>, delivery_id: Uuid, now: i64) -> Result<(), SendError>`. There is no wrapper and no `send_one` or `send_issues` afterwards: every caller builds the enum. Update every caller in this phase: `routes/channels.rs` (the import and the test-send call: `Outgoing::Alert(&notice)`), `attempt_of` below, and every call in `alerts/deliver/tests.rs` (find them with `grep -rn "send_one" crates/server/src`). `Outgoing` is `pub(crate)` (`#[derive(Clone, Copy)]`).
3. `send` is today's `send_one` body with the three body builders chosen by the enum: Slack `message::slack_body(n)` / `message::issues_slack_body(n)`; webhook `message::webhook_body(n)` / `message::issues_webhook_body(n)`; email: `send_email(state, org_id, recipients, &email, now)` where `email` is built once (`message::email(n)` / `message::issues_email(n)`) and `send_email`'s parameter becomes `email: &Email` instead of `notice: &Notice` (its quota, member and error logic are unchanged).
4. `attempt_of`: `match &due.body { DueBody::Rule(event) => send(.., Outgoing::Alert(&notice_of(state, due, event)), ..), DueBody::Issues(issues) => send(.., Outgoing::Issues(&issues_notice_of(state, due, issues)), ..) }` (bind each notice to a local first so the borrow lives through the call). `notice_of` takes the event as a parameter (it read `due.event`); the telemetry call and the retry logic after it are unchanged.
5. `issues_notice_of(state, due, issues) -> IssuesNotice`: `issue_urls` by `message::issue_url(public_url, &due.org_slug, &due.project_slug, item.issue_id)` per item, `issues_url` by `message::issues_url(.., issues.source_id)`, the rest copied from `issues` and `due`.
6. Logs: unchanged lines (`"alert delivery"` with delivery id, channel, attempt, outcome). Never log titles.

**Tests** (`alerts/deliver/tests.rs`, using its helpers; add helpers `insert_issue_notification(app, digest_json, created_at) -> Uuid` and `insert_issue_delivery(app, notification, channel, status, next_attempt_at, updated_at) -> Uuid`; the source and issue rows come from the helpers of `crate::test_support::issues`, M4.3: `source_row`/`put_source`, `issue_row`/`put_issue`; the project from the seed):
- `due_deliveries_returns_rule_and_issue_deliveries_ordered_together`: two of each kind with interleaved `next_attempt_at`; the order is by `(next_attempt_at, id)` across kinds.
- `due_deliveries_limit_applies_to_the_union`: limit 3 over 2+2 rows returns the 3 oldest.
- `due_deliveries_skips_inactive_projects_for_issue_deliveries` (project `state = 'deleting'`) and `…skips_inactive_orgs`.
- `due_deliveries_skips_sent_and_future_issue_deliveries`.
- `an_issue_delivery_is_sent_to_a_webhook`: a `HookReceiver`, a webhook channel, one issue notification and delivery; `deliver_once(&app.state, now)`: one request, `X-Cetra-Signature` and `X-Cetra-Delivery` present and valid, body `type == "issues.detected"`, `items` and `issues_url` as inserted, delivery `sent`.
- `an_issue_delivery_is_sent_to_slack_and_email` (Slack through a `HookReceiver`; email through the test mailer the existing email-delivery tests use): the Slack header and the email subject match the notification.
- `an_issue_delivery_retries_and_fails_like_a_rule_delivery`: a receiver answering 500 gives `Retry` with the K backoff; at `attempts + 1 == MAX_ATTEMPTS` it fails.
- `an_issue_email_counts_against_the_daily_quota` (the `alert_email_count` reservation is taken per recipient, as for alerts).
- `prune_removes_old_issue_notifications_and_their_deliveries`: a notification older than 90 days is deleted with its deliveries (cascade); a newer one stays; finished deliveries older than 30 days are deleted as before; the returned triple is right.
- `deleting_a_channel_removes_its_issue_deliveries` (cascade).
- `the_notice_debug_of_a_due_delivery_hides_titles` (`format!("{:?}", due)`).
- Existing tests for rule deliveries are updated only for the changed `prune`/`prune_once` return type.

**Done when:** `rustfmt --edition 2024` on the four files succeeds; `grep -rn "\.event\b" crates/server/src/alerts` finds no use of the removed `DueDelivery.event`; `grep -rn "send_one\|send_issues\|send_outgoing" crates/server/src` finds nothing.

**Commit:** `Deliver issue notifications`

---

## M4.9 The `notify_issues` tick

**Goal:** each alert tick turns new and regressed issue events into digests (D-IC-21).

**Edit:** new `crates/server/src/alerts/issue_notify.rs`; `crates/server/src/alerts/mod.rs`.

**Contract:**
```rust
//! Issue notifications: once per alert tick, the sources that may notify get one digest each (D-IC-21).

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NotifyReport {
    /// Sources that were due.
    pub(crate) due: usize,
    /// Notifications written.
    pub(crate) notifications: usize,
    /// Sources whose write failed.
    pub(crate) failed: usize,
}

/// Writes a digest for every enabled source of an active project and org with at least one
/// channel whose `last_notified_at` is at least 10 minutes old and that has `created` or
/// `regressed` issue events in `(last_notified_at, now]`.
///
/// # Errors
///
/// Returns [`MetaError`] only when the due sources cannot be read. A failed write is logged
/// (source id only) and counted.
pub(crate) async fn notify_issues(state: &AppState, now: i64) -> Result<NotifyReport, MetaError>
```
**Steps:**
1. `notify_issues`: `due_sources(state.meta.reader(), now)`; for each source (sequentially; there are few) first `has_new_events(state.meta.reader(), &source, now)`: `false` skips the source without taking the writer (a source with nothing to send costs one read per tick, not a write transaction); `true` runs `state.meta.write(move |conn| Box::pin(async move { notify_source(conn, &source, now).await }))`. `Sent { .. }` → count and `tracing::info!(source_id, notification_id, items, more, deliveries, "issue notification")`; `NoEvents` and `Changed` → nothing (an event can still vanish between the read and the write); `Err` → `tracing::error!(source_id = %id, error = %error, "issue notification write failed")` (the `MetaError` text holds no payload; check that it does not embed a title, as the other callers do), count `failed`.
2. `alerts/mod.rs`: `pub(crate) mod issue_notify;`. In `spawn_evaluation_loop`, after the `evaluate_once` match inside the loop body, add:
   ```rust
   match issue_notify::notify_issues(&state, now).await {
       Ok(report) if report.notifications > 0 || report.failed > 0 => tracing::debug!(due = report.due, notifications = report.notifications, failed = report.failed, "issue notification pass"),
       Ok(_) => {}
       Err(error) => tracing::error!(error = %error, "issue notification pass failed"),
   }
   ```
   Update the doc comment of `spawn_loops`/the loop ("evaluation, issue notification"). Restore mode starts no evaluation loop, so no issue notification is sent in restore mode (as for alerts).
3. `THROTTLE_NS` and `ISSUE_NOTIFY_SETTLE_NS` live in `meta/issue_links.rs` (M4.3); do not redefine them. This function binds `now` as given: the settle margin is applied inside `has_new_events` and `notify_source`. The events `at` of an issue event is its write-time `now` (M1's `errors.rs` passes `now`; M3's assign path does the same), which the margin assumes.

**Tests** (in `issue_notify.rs`, `TestApp`, the helpers of `crate::test_support::issues` (M4.3: `put_source`, `put_issue`, `put_event`, `put_channel`) to build sources, issues, events, channels; call `notify_issues(&app.state, now)` with explicit `now`; **every event's `at` is at least `ISSUE_NOTIFY_SETTLE_NS` + 1 ns before `now`**, except in the margin test):
- `a_source_with_events_and_channels_notifies_once`: events after the cut-off make one notification with one delivery per channel; `last_notified_at` becomes the newest event's `at`; a second call at the same `now` writes nothing.
- `throttle_is_ten_minutes`: after a notification, a new event at `last + 1 min` is not sent at `last + 10 min - 1 ns`, and is sent at `last + 10 min` (it is older than the margin by then).
- `an_event_inside_the_settle_margin_waits_for_the_next_tick`: an event at `now - 30 s` is not sent at `now`; the same call at `now + 1 min` sends it.
- `no_events_means_no_notification`: nothing written, `last_notified_at` unchanged.
- `a_source_without_channels_never_notifies` and `a_disabled_source_never_notifies`.
- `a_paused_source_still_notifies`: `paused_reason` set; the digest is written.
- `an_inactive_project_or_org_never_notifies`.
- `enabling_a_source_sets_the_cutoff_so_the_backlog_is_not_sent`: a source whose issues' events predate the enabling (the route sets `last_notified_at = now`) sends nothing; only later events are sent.
- `regressed_events_are_sent_with_their_kind`: the digest holds `regressed` for a regression event and `created` for a new issue.
- `the_digest_names_up_to_twenty_and_counts_the_rest` (25 events: 20 items, `more` 5, newest first).
- `user_events_never_notify`: `state_changed`, `renamed` and `merged` events are not sent.
- `two_sources_notify_independently`.
- `m3_assign_events_are_covered`: write the events the way M3's assign path does (`put_event` with `EventKind::Created`, actor `None`, `at` = the write time) and assert they are collected.

**Done when:** `rustfmt --edition 2024` on the two files succeeds.

**Commit:** `Send issue notification digests on the alert tick`

---

## M4.10a Shared issue reads

**Goal:** move the read helpers MCP and the dataset route need out of `routes/issues.rs` (D-IC-24 "moved from `routes/issues.rs` to `issues/` as `pub(crate)`"), without changing any answer.

**Edit:** new `crates/server/src/issues/read.rs`; `crates/server/src/issues/mod.rs` (one line `pub(crate) mod read;`); `crates/server/src/routes/issues.rs` (including its tests: the private test `issue_row` and `put_issue` are replaced by the imports from `crate::test_support::issues`, M4.3; its private hours-based `occurrence` stays).

**Contract** (`issues/read.rs`; the bodies are today's, moved):
```rust
/// Occurrence count and user count of a name set (D-IC-9).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally { pub(crate) count: u64, pub(crate) users: u64 }

/// How many issues' occurrences an issue page shows or copies: the issue and 7 merged ones (the former `MAX_EXAMPLE_NAMES`, 8).
pub(crate) const MAX_EXAMPLE_NAMES: usize = 8;
/// The default occurrences period, 14 days (the former `OCCURRENCE_DEFAULT_PERIOD_NS`).
pub(crate) const OCCURRENCE_DEFAULT_PERIOD_NS: i64 = 14 * DAY_NS;

pub(crate) fn score_name(issue: Uuid) -> String
/// The occurrence count of an issue's own name in `counts`; 0 when it has none.
pub(crate) fn issue_count(counts: &HashMap<String, Tally>, issue: Uuid) -> u64
/// Orders merged issues by their occurrence count, most first, then by id (D-IC-9).
pub(crate) fn most_occurrences_first(merged: &mut [Uuid], counts: &HashMap<String, Tally>)
/// The issues whose occurrences an issue page shows or copies: `id` first, then `merged` ordered by `most_occurrences_first` over `counts`, at most [`MAX_EXAMPLE_NAMES`] in all.
pub(crate) fn example_issues(id: Uuid, merged: &[Uuid], counts: &HashMap<String, Tally>) -> Vec<Uuid>
pub(crate) fn names_of(issue: Uuid, merged: &[Uuid]) -> Vec<String>
pub(crate) fn total(names: &[String], tallies: &HashMap<String, Tally>) -> Tally
pub(crate) async fn name_counts(ctx: &QueryContext, names: &[String], window: TimeWindow, by_day: bool) -> Result<Vec<ScoreNameCount>, ApiError>
pub(crate) async fn tallies(state: &AppState, org: OrgId, project: ProjectId, names: &[String], window: TimeWindow) -> Result<HashMap<String, Tally>, ApiError>

/// The newest occurrence scores of each issue in `issues` (name `issue:<id>`, source `issue`),
/// up to `per_issue` each, as `(issue, score)` pairs, issue by issue in the given order and
/// newest first inside an issue. One storage read per issue.
pub(crate) async fn occurrence_scores(
    ctx: &QueryContext,
    project: ProjectId,
    issues: &[Uuid],
    per_issue: usize,
) -> Result<Vec<(Uuid, Score)>, ApiError>
```
**Steps:**
1. Move `Tally`, `score_name`, `issue_count`, `most_occurrences_first`, `names_of`, `total`, `name_counts`, `tallies`, and the constants `MAX_EXAMPLE_NAMES` and `OCCURRENCE_DEFAULT_PERIOD_NS` verbatim from `routes/issues.rs` to `issues/read.rs` (`DAY_NS` from `cetra_core`/where `routes/issues.rs` imports it) (with their doc comments and the `READ_TIMEOUT`/`open_query` imports from `crate::routes::read`); delete them from `routes/issues.rs` and import them (`use crate::issues::read::{…}`). Move their tests too, if any test targets them directly.
2. `occurrence_scores`: the loop body of `get_issue_occurrences` that calls `cetra_storage::score_list` with `name: Some(name.as_str())`, `source: Some(ScoreSource::Issue)`, `limit: per_issue` and converts with `convert::scores(&page.batches, project)`; it returns `(issue, score)` for every converted score. `get_issue_occurrences` then builds its list with `example_issues(id, &merged, &by_name)` (it replaces the `by_count`/`truncate(MAX_EXAMPLE_NAMES)` lines: same result), calls `occurrence_scores` with it and `EXAMPLES_PER_NAME`, and builds the examples with `example_of(score, issue)` as today (sorting and truncation unchanged). The dataset route (M4.11) ranks with the same default window.
3. No behaviour change: every existing test of `routes/issues.rs` must still describe the same answers.

**Tests:** the moved tests stay green by construction; add in `issues/read.rs`:
- `occurrence_scores_returns_newest_first_per_issue_up_to_the_limit`: an issue with 5 occurrence scores and `per_issue = 3` returns the 3 newest; two issues return their pairs in the given order.
- `occurrence_scores_of_an_issue_without_occurrences_is_empty`.
- `names_of_puts_the_issue_first`; `total_sums_counts_and_takes_the_largest_user_count`.
- `example_issues_puts_the_issue_first_then_the_busiest_merged_ones`: an issue with 9 merged issues of distinct counts returns 8 ids: the issue, then the 7 with the most occurrences, most first; two merged issues with equal counts are ordered by id; fewer than 8 issues are all returned.

**Done when:** `rustfmt --edition 2024` on the four files succeeds; `grep -n "fn tallies\|fn names_of\|struct Tally\|fn most_occurrences_first\|fn issue_count" crates/server/src/routes/issues.rs` finds nothing.

**Commit:** `Move the issue count helpers out of the routes`

---

## M4.10b MCP `list_issues`

**Goal:** the eighth tool (D-IC-24).

**Edit:** new `crates/server/src/mcp/issues.rs`; `crates/server/src/mcp/tools.rs`; `crates/server/src/mcp/mod.rs`.

**Contract:**
- Name: `pub(crate) const LIST_ISSUES: &str = "list_issues";` in `mcp/issues.rs`; the tool is the **last** of `tool_list()` (index 7).
- Arguments:
  ```rust
  #[derive(Debug, Deserialize)]
  #[serde(deny_unknown_fields)]
  struct ListIssuesArguments {
      project: String,
      state: Option<StateArg>,
      source: Option<String>,
      limit: Option<i64>,
      max_chars: Option<i64>,
  }
  #[derive(Debug, Clone, Copy, Deserialize)]
  #[serde(rename_all = "snake_case")]
  enum StateArg { Open, Resolved, Ignored, All }
  ```
- Input schema (`tools.rs`, in `tool_list()` after `get_span`):
  ```rust
  tool(
      super::issues::LIST_ISSUES,
      "List issues",
      "Issues of a project (recurring errors and, from LLM sources, groups of similar traces), newest `last_seen` first, with their occurrence counts over the last 7 days. `state` is open (default), resolved, ignored or all (never merged issues). `source` is a source id (each issue's `source_id`). `url` opens the issue page.",
      json!({
          "type": "object",
          "properties": {
              "project": {"type": "string", "description": PROJECT_ARG},
              "state": {"type": "string", "enum": ["open", "resolved", "ignored", "all"], "default": "open", "description": "Only issues in this state."},
              "source": {"type": "string", "description": "Only issues of this source id."},
              "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": "Most issues to return."},
              "max_chars": {"type": "integer", "minimum": 1000, "maximum": 100000, "default": 20000, "description": "Most characters in the result; later issues are left out beyond it."}
          },
          "required": ["project"],
          "additionalProperties": false
      }),
  )
  ```
- Output (one text item of compact JSON):
  ```json
  {"issues": [{"id": "<uuid>", "title": "<text>", "kind": "error|score|feedback|topic", "state": "open|resolved|ignored", "regressed": false,
               "source_id": "<uuid>", "first_seen": "<rfc3339>", "last_seen": "<rfc3339>", "count_7d": 12, "users_7d": 3, "url": "<issue page url>"}],
   "truncated": false, "notes": []}
  ```
  Sorted by `last_seen` descending (the store's `IssueSort::LastSeen`). Times are RFC 3339: build the items from a private `#[derive(Serialize)] struct McpIssue` whose `first_seen` and `last_seen` fields carry `#[serde(with = "cetra_core::time_serde")]` (as the API structs `mcp/traces.rs` serializes do). `url` is `crate::alerts::message::issue_url(&state.config.public_url, &org.slug, &access.project.slug, id)` with the org read by `meta::orgs::org_by_id(state.meta.reader(), access.project.org_id)` (a missing org is an internal error).
- Permission: `user_project(state, user, &args.project, ProjectPerm::View)`.

**Steps:**
1. `mcp/issues.rs`: `pub(crate) async fn list_issues(state, user, raw) -> Result<Value, ApiError>`. Order:
   1. `let args: ListIssuesArguments = tools::arguments(raw)?;`
   2. access (above); `let max_chars = output::max_chars(args.max_chars)?;`
   3. `limit`: `None` → 20; outside `1..=100` → `ApiError::BadRequest { detail: Some("limit must be between 1 and 100".into()), errors: vec![] }`.
   4. `source`: `None`, or parse as `Uuid` (error `detail: "source must be a source id"`).
   5. `states`: `Open` (default) → `[IssueState::Open]`, `Resolved`, `Ignored`, `All` → `[Open, Resolved, Ignored]`.
   6. `let mut rows = meta::issues::list_issues(state.meta.reader(), project, &IssueListQuery { states: &states, source, q: None, ids: None, sort: IssueSort::LastSeen, cursor: None, limit })`; it returns up to `limit + 1` rows: `let more = rows.len() > limit as usize; rows.truncate(limit as usize);`.
   7. Sources by id (one `list_sources` read, as `routes/issues.rs::sources_of` does; use a small local map) for `kind`.
   8. `merged_ids` per row, `names_of` (all from `crate::issues::read`), then `tallies(state, org, project, &names, window)` with `window = TimeWindow::new(now - 7 days, now)` (`now = state.clock.now_ns()`), and `total(...)` per row for `count_7d`/`users_7d`.
   9. Build the body; `notes` gets `"more issues exist than limit; raise limit or narrow state/source"` when `more`; `truncated` is `more`; then (the body is a `serde_json::Map`, as `fit_list` takes `&mut serde_json::Map`; build it as a `Map`, not a `Value`) `output::fit_list(&mut body, "issues", max_chars, &format!("result cut at {max_chars} characters; later issues left out"))` (this sets `truncated` true and adds its note when it cuts).
2. `mcp/mod.rs`: `mod issues;`; in `run_tool`, before the `_` arm: `issues::LIST_ISSUES => (issues::LIST_ISSUES, issues::list_issues(&self.state, &self.user, arguments).await),`. Update `INSTRUCTIONS`: "Read-only access to LLM traces and spans, and to the issues found in them." Update the doc comment of `run_tool`/module header if they say "seven".
3. Update the module doc of `tools.rs` ("D-H-11") only if it lists tools.

**Tests** (in `mcp/mod.rs` tests for the list, in `mcp/issues.rs` for the tool; helpers `test_helpers::{app_with_token, call}`):
- `tools_list_has_every_tool_read_only` (existing): `TOOLS` becomes `[&str; 8]` with `"list_issues"` last (also used by the `server/discover` test at the second `assert_eq!(names, TOOLS)`); add `assert_eq!(tools[7]["inputSchema"]["required"], json!(["project"]));`.
- `list_issues_defaults_to_open_issues_newest_first`: three issues (two open, one resolved) with distinct `last_seen`; the open two come in `last_seen` descending order; every field of the output is present; `url` ends with `/issues/<id>`.
- `list_issues_counts_the_last_seven_days`: occurrences inside and outside 7 days give `count_7d` and `users_7d` of the inside ones; a merged issue's occurrences count on its target.
- `list_issues_state_filters`: `resolved`, `ignored`, `all` (all three, no merged).
- `list_issues_filters_by_source`.
- `list_issues_limit_and_truncation`: 3 issues with `limit 2` give 2, `truncated true`, a note; `limit 0` and `limit 101` are `isError` with the 400 text; a bad `source` is an error.
- `list_issues_respects_max_chars`: a small `max_chars` cuts the list and sets `truncated` with the note.
- `list_issues_needs_project_view`: a project the token's user cannot see answers as every other tool does (404 problem).
- `list_issues_unknown_argument_is_refused` (`deny_unknown_fields`).
- `list_issues_is_read_only`: covered by the annotations loop of `tools_list_has_every_tool_read_only`.

**Done when:** `rustfmt --edition 2024` on the three files succeeds; `grep -rn "seven\|Seven" crates/server/src/mcp` finds nothing.

**Commit:** `Add the list_issues MCP tool`

---

## M4.11 Add examples to a dataset (server)

**Goal:** `POST /api/v1/projects/{project}/issues/{id}/dataset-items` (D-IC-22), through the shared bulk-add core.

**Edit:** `crates/server/src/routes/datasets.rs`; `crates/server/src/api/datasets.rs`; new `crates/server/src/routes/issue_dataset_items.rs`; `crates/server/src/routes/mod.rs` (`pub(crate) mod issue_dataset_items;`); `crates/server/src/http/access.rs` (`routes.extend(crate::routes::issue_dataset_items::routes());` after the issues line); `crates/server/src/api/openapi.rs`.

**Contract:**
```rust
// api/datasets.rs
/// `POST /api/v1/projects/{project}/issues/{id}/dataset-items` body (D-IC-22).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueDatasetItemsRequest {
    /// The dataset: its id or its name.
    pub dataset: String,
    /// Most traces to add, 1 to 100. Default 100.
    #[serde(default, deserialize_with = "super::online_rules::present")]
    pub limit: Option<u32>,
}

// routes/datasets.rs
/// The core of both bulk adds: copies the traces of `items` into `dataset` as one new version
/// (D-J-25), giving every item `metadata`.
pub(crate) async fn add_trace_refs(
    state: &AppState,
    principal: &Principal,
    access: &ProjectAccess,
    dataset: &str,
    items: &[TraceRef],
    metadata: &Map<String, Value>,
) -> Result<FromTracesResult, ApiError>
```
The answer of the new route is J's `FromTracesResult` with `201`.

`api/datasets.rs` has its own private `present` returning `Option<Option<T>>`; the `Option<T>` variant is `super::online_rules::present` (`pub(super)`), as `issues.rs` uses it, so `"limit": null` is refused (400) like other optional fields.

**Steps:**
1. `routes/datasets.rs`: extract the body of `add_from_traces` (from the batch-size check to the `FromTracesResult` it builds) into `add_trace_refs`, replacing `body.items` by `items` and `&dataset` by `dataset`. The only change in the logic: the `FromTraceRequest` built for `item_from` gets `metadata: (!metadata.is_empty()).then(|| metadata.clone())`. The audit log line moves into `add_trace_refs` (so both routes keep it, written once); the conversion of a skipped trace reads its id from `items[index].trace_id`. `add_from_traces` becomes: call `add_trace_refs(&state, &principal, &access, &dataset, &body.items, &Map::new())` and wrap in `(StatusCode::CREATED, error::Json(result))`. Its doc comment, route, body limit and errors stay: **J's route is unchanged** (its tests are not edited).
2. `routes/issue_dataset_items.rs`: `pub(crate) fn routes() -> Vec<RouteDef>` with one `RouteDef { method: POST, path: "/api/v1/projects/{project}/issues/{id}/dataset-items", access: Access::Project { perm: ProjectPerm::ManageDatasets, session_only: false }, documented: true, handler: post(add_issue_examples) }`. Handler `add_issue_examples(State(state), Extension(principal), Extension(access), error::Path(IssuePath { id }), error::Json(body): error::Json<IssueDatasetItemsRequest>)` (`#[derive(Deserialize)] struct IssuePath { id: Uuid }`), in this order:
   1. `limit = body.limit.unwrap_or(100)`; outside `1..=100` → `ApiError::BadRequest` with the field error `limit`: "must be between 1 and 100". A blank `dataset` → field error `dataset`: "must not be empty".
   2. The issue: `meta::issues::get_issue(state.meta.reader(), project, id)`; `None` → `ApiError::NotFound`. Any state is allowed.
   3. The group (Q2): `merged = merged_ids(...)`; `names = names_of(id, &merged)`; `window = TimeWindow::new(now - OCCURRENCE_DEFAULT_PERIOD_NS, now)` with `now = state.clock.now_ns()` (the default window of `get_issue_occurrences`, 14 days ending now); `counts = tallies(&state, org, project, &names, window).await?`; `group = example_issues(id, &merged, &counts)` (the issue, then the 7 merged issues with the most occurrences, at most 8 ids; all from `crate::issues::read`).
   4. `let ctx = open_query(&state, org, project, None).await?;` then `occurrence_scores(&ctx, project, &group, limit as usize)` (M4.10a; `limit` rows per issue, so an issue whose occurrences share traces can leave fewer than `limit` distinct traces although more exist: accepted). Keep only scores whose target is `ScoreTarget::Trace { trace_id }`; sort by `created_at` descending then `trace_id` descending; keep the first score of each trace id (distinct traces); take `limit`.
   5. No score left → `ApiError::BadRequest` with the field error `issue`: "the issue has no occurrences in the data window" (Q3).
   6. `TraceRef { trace_id: trace_id.to_string(), span_id: score.metadata.get("cetra.issue.span_id").cloned() }` per kept score (trace root when the key is absent).
   7. `metadata = {"cetra.issue_id": "<id of the requested issue>"}` (a `serde_json::Map`).
   8. `add_trace_refs(&state, &principal, &access, &body.dataset, &refs, &metadata).await?` and answer `(StatusCode::CREATED, error::Json(result))`. When every trace is skipped, J's core answers its 400 whose field errors are named `items[i]` (index in `refs`); the issue route passes them through unchanged (accepted).
   Log with ids and counts only: the audit line is `add_trace_refs`' own; add no other log.
3. `api/openapi.rs`: import `IssueDatasetItemsRequest`; `set.insert::<IssueDatasetItemsRequest>();` next to `FromTracesRequest`; in the issues section (where `merge` is built, so `id` is in scope) add
   ```rust
   let dataset_items = operation("post_issue_dataset_items", "issues", "Add an issue's occurrences to a dataset")
       .description(Some("Project members and above, a read-write personal access token, or an API key with `write` (D-IC-22). Copies the most recent occurrence traces of the issue and of the issues merged into it, one item per trace and up to `limit` (default and maximum 100), into the dataset as one new version, as `items/from-traces` does. An occurrence with an error span uses that span, else the trace root. Items get the metadata `cetra.issue_id`. A trace that cannot be copied is listed in `skipped`; no occurrence in the data window or no copyable trace is a 400."))
       .parameters(Some(vec![project_param(), id.clone()]))
       .request_body(Some(json_body("IssueDatasetItemsRequest")))
       .responses(responses("201", json_of::<FromTracesResult>("The new version, the items added and the traces skipped."), &[400, 401, 403, 404, 409, 429, 503]))
       .build();
   ```
   and the path `format!("{PROJECT}/issues/{{id}}/dataset-items")` with `PathItem::new(HttpMethod::Post, dataset_items)` after the `merge` path. Do not regenerate `openapi.json`.

**Tests** (in `routes/issue_dataset_items.rs`, `TestApp`; build sources, issues and occurrence scores with the `crate::test_support::issues` helpers (M4.3: `put_source`, `put_issue`, `occurrence`), with spans committed through `app.commit_spans` so the traces load; a dataset through the datasets route):
- `adds_the_newest_distinct_traces`: 3 occurrences on 2 traces (two issues of one group on the same trace) give 2 items; order newest first; one version holds them.
- `limit_caps_the_items`: 5 traces, `limit 2` adds the 2 newest; the default adds all 5; `limit 0` and `limit 101` are 400 on `limit`.
- `uses_the_occurrence_span_when_present`: an occurrence whose score metadata has `cetra.issue.span_id` copies that span's input/output; one without the key copies the trace root. Item `source_span_id` shows which.
- `items_carry_the_issue_id_metadata`: every item's `metadata == {"cetra.issue_id": "<issue id>"}`, with the requested (target) issue's id, also when the occurrence belongs to a merged issue.
- `merged_issues_count_for_their_target`: an occurrence of a merged issue is added when the target is requested.
- `the_group_is_the_issue_and_the_seven_busiest_merged_issues`: an issue with 9 merged issues, each with one occurrence on its own trace, and occurrence counts that differ (the busier ones get extra occurrences on other traces inside the 14 days); the items come from the issue and the 7 merged issues with the most occurrences, the 2 least busy are absent; an occurrence older than 14 days does not count toward the ranking.
- `any_state_is_allowed` (resolved, ignored); `a_missing_issue_is_404`; `an_issue_of_another_project_is_404`.
- `an_issue_without_occurrences_is_400`: field `issue`, message "the issue has no occurrences in the data window".
- `a_null_limit_is_400`.
- `a_dataset_by_name_works`; `an_unknown_dataset_is_404`.
- `skipped_traces_are_listed`: an occurrence whose trace is gone (not committed) comes back in `skipped` with the J message, and the others are added; when every trace is skipped the answer is the 400 of J's route (assert the status and that a field error is present).
- `a_viewer_and_an_annotator_cannot_add` (403; `ManageDatasets` is member+); `a_read_only_key_cannot_add`.
- `the_j_route_is_unchanged` (in `routes/datasets.rs` tests, one new test): `items/from-traces` still answers 201 and items have `metadata == {}`. All existing J tests stay untouched.

**Done when:** `rustfmt --edition 2024` on every edited `.rs` file succeeds; `add_from_traces` contains no copy of the loop.

**Commit:** `Add examples from an issue to a dataset`

---

## M4.12a Web: alert library

**Goal:** the form model, labels and validation know `issue_occurrences`.

**Edit:** `web/src/lib/alerts.ts`; `web/src/lib/alerts.test.ts`; `web/src/lib/search/alerts.ts`.

**API contract** (it appears in the regenerated `schema.gen.ts`; write against it): `Schema<"AlertMetric">` gains `"issue_occurrences"`; `Schema<"AlertCondition">` gains optional `issue_id?: string` and `source_id?: string`.

**Steps** (every `switch (condition.metric)` / `switch (metric)` in the file gets an `issue_occurrences` arm; TypeScript fails the build otherwise):
1. `metricLabel`: `"Issue occurrences"`.
2. `formatAlertValue`: `String(Math.round(value))` (a count).
3. `thresholdUnit`: `"occurrences"`. `thresholdInPercent` is unchanged (false).
4. `metricDefaults("issue_occurrences")`: `{ threshold: "0", comparator: "gt", minCount: DEFAULT_RULE_FORM.minCount }`.
5. `RuleFormValues` gains `issueScope: "issue" | "source"`, `issueId: string`, `sourceId: string`; `DEFAULT_RULE_FORM` gets `"issue"`, `""`, `""`. Export `type IssueScope = RuleFormValues["issueScope"]`.
6. `formFromRule`: `issueScope: condition.source_id !== undefined ? "source" : "issue"`, `issueId: condition.issue_id ?? ""`, `sourceId: condition.source_id ?? ""`; `minCount` default for the metric as for `cost`.
7. `buildCondition`: for `issue_occurrences`: no `filter`, no `min_count`; set `issue_id` (scope `issue`) or `source_id` (scope `source`), trimmed. `min_count` is omitted for `cost` **and** `issue_occurrences`.
8. `conditionFromForm`: window: for `issue_occurrences`, window `1m` or `5m` gives `errors["condition.window"] = "Issue occurrences need a window of 15 minutes or more."`; threshold: for `issue_occurrences` a number `< 0` gives `errors["condition.threshold"] = "Enter 0 or more."`; min count is not validated; scope: scope `issue` with a blank `issueId` gives `errors["condition.issue_id"] = "Choose an issue."`, scope `source` with a blank `sourceId` gives `errors["condition.source_id"] = "Choose a source."`.
9. `previewCondition` follows `buildCondition`. `previewQuery`: `case "issue_occurrences": return undefined;` (Q6).
10. `conditionChanged`: metric, window, comparator and threshold always count; for `issue_occurrences`, `filter` and `minCount` are ignored, and the scope counts (`issueScope`, then `issueId` or `sourceId` for the chosen scope); other metrics ignore the three new fields.
11. `lib/search/alerts.ts`: add `export type NewRuleSearch = { metric?: "issue_occurrences"; issue_id?: string; source_id?: string };` and `parseNewRuleSearch(search: Record<string, unknown>): NewRuleSearch`: keeps `metric` only when it is exactly `"issue_occurrences"`, and `issue_id` / `source_id` only when they are non-empty strings (mirror the style of `parseAlertsSearch` in the same file).

**Tests** (`lib/alerts.test.ts`, existing style):
- `metricLabel`, `formatAlertValue`, `conditionSummary` for `issue_occurrences` (`"Issue occurrences > 0 over 1h"`, value `"12"`).
- `metricDefaults`/`thresholdUnit` for the new metric.
- `conditionFromForm` for an issue scope (window `1h`): builds `{ metric, window, comparator, threshold, issue_id }` with no `min_count` and no `filter`, even when `filter` and `minCount` hold text.
- `conditionFromForm` with window `1m` or `5m` and metric `issue_occurrences` gives the `condition.window` error; `15m` and `1h` pass; other metrics accept `1m`.
- `conditionFromForm` for a source scope builds `source_id`; both scopes' blank-id errors; a negative threshold error; a non-integer threshold (`2.5`) is accepted.
- `formFromRule` round-trips an issue rule and a source rule.
- `conditionChanged`: changing only the filter of an issue rule is not a change; changing the issue id is; switching metric and back is not.
- `previewQuery` returns `undefined`.
- `parseNewRuleSearch`: keeps valid values; drops an unknown `metric`, empty strings and non-strings.

**Done when:** `pnpm --dir web exec biome format --write` on the three files succeeds; by hand, no `switch` over the metric lacks the new case.

**Commit:** `Teach the alert library about issue occurrences`

---

## M4.12b Web: rule form

**Goal:** the rule form creates and edits `issue_occurrences` rules, with a scope picker and a prefill (D-IC-23).

**Edit:** `web/src/components/alerts/rule-form.tsx`; `web/src/components/alerts/rule-preview.tsx`; `web/src/components/alerts/rule-detail.tsx`; new `web/src/components/alerts/channel-checkboxes.tsx`; new `web/src/components/alerts/issue-scope-fields.tsx`; new `web/src/components/alerts/select-field.tsx`; `web/src/routes/$org/$project/alerts/new.tsx`; `web/src/routes/$org/$project/alerts/new.test.tsx`; `web/src/routes/$org/$project/alerts/$ruleId/edit.test.tsx`.

**Contracts:**
```tsx
// channel-checkboxes.tsx — the Notifications fieldset body of RuleForm, extracted (the source form reuses it)
export function ChannelCheckboxes(props: {
  org: string;
  project: string;
  idPrefix: string;                 // "rule" keeps today's ids: `rule-channel-<id>`, `rule-channels-error`
  selected: string[];
  onToggle: (id: string, checked: boolean) => void;
  disabled?: boolean;
  limit?: number;                   // default 10 (CHANNEL_LIMIT)
  error?: string;
}): JSX.Element
// issue-scope-fields.tsx
export function IssueScopeFields(props: {
  org: string;
  project: string;
  scope: IssueScope;
  issueId: string;
  sourceId: string;
  errors: Record<string, string>;
  disabled?: boolean;
  onChange: (patch: Partial<Pick<RuleFormValues, "issueScope" | "issueId" | "sourceId">>) => void;
}): JSX.Element
// RuleForm gains an optional prop
initial?: Partial<RuleFormValues>;   // create only: merged over DEFAULT_RULE_FORM
```
**Steps:**
1. `channel-checkboxes.tsx`: move the Notifications fieldset's content (the channels query, skeleton, "No channels yet. Add a channel" link, the checkbox list with `channelKindLabel`, the limit, the error text) out of `rule-form.tsx` unchanged in markup and ids (so existing tests keep passing). `RuleForm` renders `<ChannelCheckboxes idPrefix="rule" … />` inside its fieldset (the `legend` and `aria-describedby` stay in the form).
2. `select-field.tsx`: move the private `SelectField` function (and its `Option` type if it is local) of `rule-form.tsx` there, exported unchanged; `rule-form.tsx` and `issue-scope-fields.tsx` both import it from it (no import cycle).
   `issue-scope-fields.tsx`: a select (via `SelectField`) labelled **Scope** with options "One issue" (`issue`) and "All issues of a source" (`source`), then:
   - scope `issue`: a select labelled **Issue** listing `GET /api/v1/projects/{project}/issues?state=all&limit=100` (use the query helpers of `web/src/api/queries-issues.ts`; read the file and use the list query it exports, or `$api.useQuery` with `query: { state: "all", limit: 100 }`); each option shows the title (cut to 60 characters) and the source name; when `issueId` is set and not in the page, fetch it with `issueQuery(org, project, issueId)` and add its option (so a prefilled issue always shows); error `errors["condition.issue_id"]`;
   - scope `source`: a select labelled **Source** listing the project's issue sources (the sources list query of `queries-issues.ts`); error `errors["condition.source_id"]`;
   - description text: "Counts occurrences in the window, including the issues merged into it." (issue) / "Counts occurrences of every issue of the source that is not ignored or merged." (source).
3. `rule-form.tsx`:
   - `METRIC_OPTIONS` gets `{ value: "issue_occurrences", label: "Issue occurrences" }` (last).
   - `FORM_FIELDS` gets `"condition.issue_id"` and `"condition.source_id"`.
   - In the "What to measure" fieldset, `form.metric === "issue_occurrences"` renders `<IssueScopeFields … />` (before the window select).
   - The Filter block and the "Minimum samples" field are hidden for `issue_occurrences` (the filter value is ignored by `buildCondition`).
   - `thresholdHint`: unit `"occurrences"` → `"Occurrences."`.
   - Window (D-IC-20: `15m` or more): when the metric is `issue_occurrences` the window select lists only the options `15m`, `1h`, `6h`, `24h` (filter `WINDOW_OPTIONS`), and `changeMetric` switching to `issue_occurrences` while the window is `1m` or `5m` sets the window to `1h`.
   - `changeMetric` unchanged (it uses `metricDefaults`).
   - `useState` initial: `rule ? formFromRule(rule) : { ...DEFAULT_RULE_FORM, ...props.initial }`.
   - `rule-preview.tsx`: when `previewQuery(values)` is `undefined` the component today says "No preview for LLM span latency. The dashboard shows it per model.". Make the message depend on the metric: for `issue_occurrences` render "No preview for issue occurrences." in the same box; keep the LLM-latency text for the other case.
   - `rule-detail.tsx`: for a rule whose condition has `issue_id` or `source_id`, add a row **Scope** next to the condition summary: a link to the issue page (text: the issue's title cut to 60 characters, from the issue query of `queries-issues.ts`, falling back to `shortId(issue_id)` while loading or on error) or to the source page (`/$org/$project/issue-sources/$sourceId`, text: the source's name, same fallback).
4. `alerts/new.tsx`: `validateSearch: (search) => parseNewRuleSearch(search)`; `const search = Route.useSearch();` and pass `initial` to `RuleForm` when `search.metric === "issue_occurrences"`: `{ metric: "issue_occurrences", issueScope: search.source_id ? "source" : "issue", issueId: search.issue_id ?? "", sourceId: search.source_id ?? "", window: "1h", comparator: "gt", threshold: "0" }` (D-IC-23 defaults). The name is left empty. Prefill only: the user saves.

**Tests:**
- `new.test.tsx` (MSW, as the file's existing tests): `creates an issue occurrences rule`: choose Metric "Issue occurrences" (the window changes from 5 minutes to 1 hour), the Scope "One issue", an issue from a mocked `issues` list, type a name, submit: the body has `condition: { metric: "issue_occurrences", window: "1h", comparator: "gt", threshold: 0, issue_id }` and no `min_count`/`filter`.
- `the window list has no 1 or 5 minutes for issue occurrences`.
- `the preview says there is none for issue occurrences` and the rule detail (in `rule-panel.test.tsx` if that file renders `rule-detail`, else the `$ruleId/index.test.tsx`; add the case to whichever test renders it, listed in the report) shows the Scope row with the issue title.
- `creates a source rule`: scope "All issues of a source", pick a source; body has `source_id`.
- `prefills from the search params`: render `PAGE + "?metric=issue_occurrences&issue_id=<id>"`: the metric select shows "Issue occurrences", the window "1 hour", the threshold `0`, the issue select shows that issue (mock `GET /issues/{id}` for the fetch), nothing is saved until Create.
- `an unknown metric in the search params is ignored` (renders the default error rate form).
- `shows the scope errors`: submit with no issue chosen: "Choose an issue." under the select and no request sent.
- `hides the filter and minimum samples for issue occurrences`.
- Existing tests (error rate, channels, errors) stay unchanged and pass through the extracted `ChannelCheckboxes`.
- `edit.test.tsx`: `edits an issue rule`: an `AlertRule` fixture with `condition.issue_id` loads the form with the issue selected; changing only the threshold sends a `condition` PATCH with `issue_id` kept.

**Done when:** `pnpm --dir web exec biome format --write` on the edited files succeeds; by hand, `rule-form.tsx` no longer contains the channel checkbox markup.

**Commit:** `Add issue occurrences to the rule form`

---

## M4.13 Web: issue page buttons

**Goal:** "Create alert" and "Add examples to dataset" on the issue page (D-IC-22, D-IC-23).

**Edit:** `web/src/components/issues/issue-actions.tsx`; `web/src/routes/$org/$project/issues/$issueId.tsx`; `web/src/routes/$org/$project/issues/$issueId.test.tsx`; `web/src/components/datasets/add-traces-to-dataset-dialog.tsx`; `web/src/components/datasets/add-traces-to-dataset-dialog.test.tsx`; `web/src/mocks/writes.ts`; `web/src/mocks/writes.test.ts` (only if its covered-operations list needs the new id).

**API contract:** the operation `post_issue_dataset_items`: `POST /api/v1/projects/{project}/issues/{id}/dataset-items`, body `{ dataset: string; limit?: number }`, `201` `FromTracesResult` (the same type `…/items/from-traces` answers).

**Steps:**
1. Dialog (`add-traces-to-dataset-dialog.tsx`). Extract the `Dialog` shell and the form into shared parts:
   ```tsx
   function DatasetTargetDialog(props: {
     open: boolean;
     onOpenChange: (open: boolean) => void;
     title: string;
     description: string;
     /** Adds to the dataset; resolves with J's result. */
     add: (datasetId: string) => Promise<Schema<"FromTracesResult">>;
     /** True while that add request runs (the caller's mutation `isPending`). */
     adding: boolean;
     /** Optional: the traces list clears its selection; the issue dialog passes none. */
     onAdded?: () => void;
   }): JSX.Element
   ```
   `DatasetTargetDialog` owns `createDataset = useCreateDataset()` and `busy = createDataset.isPending || adding` (the outer `Dialog`'s close guards read it, as today) and renders `AddTracesForm` with props `{ createDataset, add, adding, onAdded, onClose }` in place of `{ traceIds, createDataset, addFromTraces, onAdded, onClose }`. In `AddTracesForm`, `onSubmit` calls `await add(target.id)` where it called `addFromTraces.mutateAsync(...)`, then `onAdded?.()`; its `busy` is `createDataset.isPending || adding`; the dataset creation, the result view with `skipped`, the invalidations and `addMessage` stay as they are. `AddTracesToDatasetDialog` keeps its exact props: it calls `useAddFromTraces()` and renders `DatasetTargetDialog` with the title and description it has today, `adding={addFromTraces.isPending}`, `add` built from that mutation (`traceIds.map((trace_id) => ({ trace_id }))`) and its `onAdded`. Add, in the same file:
   ```tsx
   export interface AddIssueExamplesDialogProps {
     open: boolean;
     onOpenChange: (open: boolean) => void;
     /** The issue whose newest occurrence traces are copied. */
     issueId: string;
   }
   export function AddIssueExamplesDialog(props: AddIssueExamplesDialogProps): JSX.Element
   ```
   It calls `$api.useMutation("post", "/api/v1/projects/{project}/issues/{id}/dataset-items")` (`adding` is its `isPending`) and renders `DatasetTargetDialog` with title `Add examples to a dataset` and description `Copies the input and output of the issue's newest occurrences (up to 100 traces, one item each) into a dataset as one new version.`; its `add` calls it with `{ params: { path: { project: projectRef(org, project), id: issueId } }, body: { dataset: datasetId } }`. A `400` on `issue` shows the server's message in the form error (the `addMessage` helper already joins field errors).
2. `issue-actions.tsx`: `IssueActions` gets props `canCreateAlert: boolean` and `canAddExamples: boolean`. For a non-merged issue, after "View traces": when `canCreateAlert`, `<Button asChild variant="outline"><Link to="/$org/$project/alerts/new" params={{ org, project }} search={{ metric: "issue_occurrences", issue_id: issue.id }}>Create alert</Link></Button>`; when `canAddExamples`, a `Button` "Add examples to dataset" opening `AddIssueExamplesDialog` (state `examplesOpen`). The component still returns `null` for merged issues.
3. `$issueId.tsx`: pass `canCreateAlert={canManageAlerts(data.role)}` and `canAddExamples={canManageDatasets(data.role)}` (both from `@/lib/roles`).
4. `mocks/writes.ts`: add to `WRITE_OPERATIONS` `{ operationId: "post_issue_dataset_items", method: "post", path: "/api/v1/projects/{project}/issues/{id}/dataset-items" }` next to `post_issue_merge`, and the handler `post_issue_dataset_items: () => HttpResponse.json(datasetItemsFromTracesDocument, { status: 201 })` (the fixture `post_dataset_items_from_traces` uses). `writes.test.ts` derives its expected list from the OpenAPI document (covered prefix `…/issues`), so it needs no edit unless it hard-codes ids: check and edit only then.

**Tests:**
- `$issueId.test.tsx`: `shows Create alert to members and not to viewers` (the link's `href` is `/acme/support-bot/alerts/new?metric=issue_occurrences&issue_id=<id>`); `shows Add examples to dataset to members only`; `hides both buttons on a merged issue`; `Create alert opens the prefilled rule form` (click, then the new-rule page shows Metric "Issue occurrences" — uses the router of `renderApp`).
- `add-traces-to-dataset-dialog.test.tsx`: `adds an issue's examples to the chosen dataset` (MSW handler on `/api/v1/projects/:project/issues/:id/dataset-items` captures the body `{ dataset: "<id>" }` and answers the fixture; the result view says `Added N items to <name>.`); `lists skipped traces from the issue route`; `shows the 400 message when the issue has no occurrences` (`issue` field error); `creates a new dataset first` (the create-then-add path works for the issue dialog); the existing `AddTracesToDatasetDialog` tests pass unchanged.

**Done when:** `pnpm --dir web exec biome format --write` on the edited files succeeds; `AddTracesToDatasetDialog`'s props are unchanged (`traces/index.tsx` is not edited).

**Commit:** `Add create alert and add examples to the issue page`

---

## M4.14 Web: source editor channels

**Goal:** the issue-source editor picks the source's notification channels (D-IC-21).

**Edit:** `web/src/components/issue-sources/source-form.tsx`; `web/src/routes/$org/$project/issue-sources/new.test.tsx`; `web/src/routes/$org/$project/issue-sources/$sourceId/edit.test.tsx` (or the existing test files of those routes; find them with `ls web/src/routes/$org/$project/issue-sources`).

**API contract:** `Schema<"IssueSource">` gains `channel_ids: string[]`; `CreateIssueSourceRequest` gains optional `channel_ids?: string[]`; `UpdateIssueSourceRequest` gains optional `channel_ids?: string[]` (read the M3 version of the form first: it already has the LLM fields).

**Steps:**
1. `FormValues` gains `channelIds: string[]`; `initialValues` reads `source.channel_ids` (`[]` for a new source).
2. `SHOWN_FIELDS` gains `"channel_ids"`.
3. A fieldset **Notifications** after the budget (or after the filter when M3's fields are in another order: put it last before the Enabled switch), legend text `Notifications`, rendering `<ChannelCheckboxes org project idPrefix="source" selected={values.channelIds} onToggle={…} disabled={pending} error={errors.channel_ids} />` and a description: `New and regressed issues are sent to these channels, in one message per source at most every 10 minutes.`
4. Create: send `channel_ids` (always, possibly `[]`). Update: send `channel_ids` only when the set changed (compare as sorted arrays, as `rule-form.tsx`'s `sameIds` does; export a shared helper only if one already exists).
5. Channels are managed in project settings; the link "Add a channel" inside `ChannelCheckboxes` stays.

**Tests:**
- `creates a source with channels`: tick two channels (`channels.json` fixture), the POST body has `channel_ids` with both ids.
- `creates a source without channels sends an empty list`.
- `edits the channels of a source`: the form shows the stored channels ticked; unticking one sends `channel_ids` with the rest and nothing else; leaving them unchanged sends no `channel_ids`.
- `shows a channel_ids error from the server` (400 `channel_ids` "unknown channel" appears under the fieldset).
- `limits the selection to 10 channels` (the 11th checkbox is disabled).
- `the notifications fieldset is shown for every kind` (error kind and an LLM kind).

**Done when:** `pnpm --dir web exec biome format --write` on the edited files succeeds.

**Commit:** `Pick notification channels in the issue source editor`

---

## M4.15 In-process tests: alerts and notifications end to end

**Goal:** the K condition and the notification chain, through the real routes, loops' functions and a webhook receiver, with the manual clock (D-IC-25).

**Edit:** new `crates/server/src/alerts/issue_links_tests.rs`; `crates/server/src/alerts/mod.rs` (add `#[cfg(test)] mod issue_links_tests;`).

**Method:** `TestApp::with_config(|config| config.security.allow_private_network_egress = true)`, `app.seed()`, a `HookReceiver::start(vec![200])`, a webhook channel and everything else through the HTTP routes with the seeded cookie (pattern: the loop test in `alerts/evaluate.rs`); sources, issues, events and channel rows come from the helpers of `crate::test_support::issues` (M4.3: `put_source`, `put_issue`, `put_event`, `occurrence`; delivering channels are created through the channels route). Events are written exactly as M1's and M3's writers write them (kind `created` or `regressed`, no actor, `at` = the write time); the writers themselves are covered by their own tests and by the CLI e2e (M4.16), which runs the real chain. **Every event time is more than `ISSUE_NOTIFY_SETTLE_NS` before the `now` of the pass that must send it.** Time moves with `app.state.clock.advance(Duration)` / `set`; passes are called directly with the clock's `now`: `evaluate::evaluate_once(&app.state, now)`, `issue_notify::notify_issues(&app.state, now)`, `deliver::deliver_once(&app.state, now)`. No `spawn_loops` (deterministic).

**Tests:**
- `an_issue_rule_fires_a_webhook_and_resolves`: create an error source and one issue with 3 occurrences, a rule `issue_occurrences` `gt 2` over `15m` with the webhook channel; `evaluate_once` → fired event; `deliver_once` → one request, `type == "alert.fired"`, `rule.metric == "issue_occurrences"`, `value == 3`, `count == 3`; advance past the window, `evaluate_once` → `resolved` event delivered.
- `a_source_rule_counts_every_live_issue_and_sees_a_new_issue`: a source rule over two issues; add a third issue's occurrence between passes: the count follows.
- `an_issue_rule_survives_a_merge`: merge the rule's issue into another: the next evaluation counts both.
- `a_deleted_source_makes_the_rule_fail_with_a_fixed_reason`: `last_error == "issue source not found"` in `GET …/alert-rules/{id}`.
- `new_issues_are_notified_in_one_digest`: a source with the webhook channel, enabled through the route (so `last_notified_at = now`); write two `created` events at distinct times after that `now`; advance the clock 11 minutes (the events are then older than the settle margin) → `notify_issues` → `deliver_once`: one request, `type == "issues.detected"`, `items` newest first with kind `created`, `more == 0`, `source_id`, `issues_url`; the signature and `X-Cetra-Delivery` headers are valid.
- `regressions_are_notified`: resolve an issue (route `PATCH`), write a `regressed` event (`put_event`), advance 11 minutes: the digest has kind `regressed`.
- `an_event_inside_the_settle_margin_waits`: an event 30 s before `now` is not in the digest sent at `now`; it is sent by the next pass after the margin.
- `the_throttle_holds_a_second_digest_for_ten_minutes`: after a digest, a new event at `+1 min` (the first digest's cut-off is its newest event's `at`): `notify_issues` at `+9 min` writes nothing; at `+10 min` after the first digest's cut-off it writes a second one (assert the two requests).
- `enabling_a_source_does_not_send_a_backlog`: issues exist before the source is enabled (created while it was disabled); enabling sets the cut-off; 11 minutes later `notify_issues` writes nothing.
- `a_disabled_source_does_not_notify_and_a_paused_one_does` (set `paused_reason` by SQL for the second).
- `a_failing_receiver_retries_with_k_backoff`: receiver answers 500 then 200; the delivery is retried after `backoff_ns(1)` and then `sent`; the notification is not duplicated.
- `a_deleted_channel_stops_notifications`: delete the channel through the route; the source's `channel_ids` is empty and `notify_issues` writes nothing; pending deliveries of the channel are gone.
- `the_email_channel_gets_the_digest_subject` (the test mailer; skip with a note if the existing helpers cannot capture mail).

**Done when:** `rustfmt --edition 2024` on the two files succeeds.

**Commit:** `Test issue alerts and notifications end to end`

---

## M4.16 CLI e2e: error issues with the real binary

**Goal:** the CLI e2e covers error issues, the lifecycle and "add examples to dataset" (D-IC-25). It does not wait for alerts (the real binary ticks every 60 s).

**Edit:** `crates/cli/tests/e2e.rs`.

**Steps:** add one test `error_issues_end_to_end`, modelled on `online_rules_and_backfill_end_to_end` (same `start(dir, extra_config)`, login, `Api`, `INIT_KEY`, `fixture_request`, `post_otlp_http`, `poll_json`, `now_ns`, final `server.running.shutdown().await`), without the mock provider:
1. Start the server. Create an `error` source: `POST /api/v1/projects/{proto_id}/issue-sources` `{"name": "Errors", "kind": "error"}` (`Api::post` returns only the JSON: assert on the body, not the status; the body has `channel_ids: []`).
2. Post an error trace: `fixture_request("sdk-observe-error", <fresh 32-hex trace id>)` with start and end times rewritten to now (copy `timed_request`'s approach for this fixture: a helper `error_request(trace_hex, start_ns)`), through `post_otlp_http` with `INIT_KEY`. Sleep 2.5 s so the sweeper tracks the project, as the template does; the trace completes after the 5 s idle timer.
3. `poll_json` (30 s; the real binary's sweeper ticks every few seconds for a rooted trace after its 5 s idle timer, as the online-rules test of this file relies on; if the existing helper's timeout proves too short, raise it for this call only and report) on `/api/v1/projects/{proto_id}/issues` until one issue with `state == "open"`; assert `kind == "error"`, `count == 1`. Then `GET …/issues/{id}/occurrences`: `examples[0].trace_id` equals the posted trace id.
4. `PATCH …/issues/{id}` `{"state": "resolved"}` → `state == "resolved"`. Post a second error trace with the same fixture (same fingerprint) starting after `resolved_at`; poll until the issue is `open` with `regressed == true` and `count == 2`.
5. Create a dataset (`POST /datasets` `{"name": "issue-examples"}`), then `POST …/issues/{id}/dataset-items` `{"dataset": "issue-examples"}`; the body has `items.len() == 2`, `skipped == []`, every item's `metadata["cetra.issue_id"] == id`; `GET …/datasets/{dataset}/items` lists the two items.
6. `POST …/issues/{id}/dataset-items` with `{"dataset": "issue-examples", "limit": 1}`: the body has one item (a second version).
7. Create a Slack channel with a placeholder `https://hooks.example.com/x` URL (never called), set it on the source with `PATCH …/issue-sources/{id}` `{"channel_ids": [<id>]}`. Assert the source reads back `channel_ids` and that `DELETE …/channels/{id}` empties it. This checks the route surface only; no notification is awaited.

Use the file's `Api` helpers (`get`, `post`, `delete`); if `Api` has no `patch`, add one next to `post` (same shape), in this file only.

**Tests:** the one test above (its numbered assertions are its checks).

**Done when:** `rustfmt --edition 2024 crates/cli/tests/e2e.rs` succeeds.

**Commit:** `Cover error issues in the CLI e2e`

---

## M4.17 Docs

**Goal:** specs, site pages, changelog and roadmap describe M4.

**Edit:**
- `docs/spec/06-evaluation.md` (§ Issues);
- `docs/spec/07-alerts.md`;
- `docs/spec/09-api.md` (§ Issues, § Evaluation & alerts, the MCP tool table);
- `docs/spec/11-ui.md` (§ Issues, the alert form);
- `website/src/content/docs/docs/platform/alerts.mdx`;
- `website/src/content/docs/docs/platform/mcp.mdx`;
- `website/src/content/docs/docs/platform/issues.mdx` (created by M3.8; read it and extend);
- `CHANGELOG.md`;
- `docs/roadmap.md`.

**Steps:**
1. Spec 06 § Issues: replace "Alerts on ignored issues are M4's." by the rule of D-IC-20 (an issue-scoped rule counts an ignored issue; a source-scoped rule skips ignored issues); add a **Links** paragraph (alerts, notifications, datasets, MCP) pointing to specs 07 and 09; mention `issue_source_channels` and the 10-minute digest (D-IC-21) in the sources paragraph.
2. Spec 07: intro "four metrics" → "five metrics"; the rule model table gets `issue_id`/`source_id` (`issue_occurrences` only, exactly one) and the "fields that do not apply" rule for the new metric (no `filter`, no `min_count`, threshold ≥ 0, zero is a value) and the window rule (`1m` and `5m` refused: 400 `condition.window`, "issue occurrences need a window of 15m or more"; an occurrence is written after its trace completes); § Evaluation: how the names are resolved at each evaluation (merge target, source scope, chunks of 1 000) and the fixed failure reasons; § Channels: the `issues.detected` webhook JSON of M4.7, the Slack and email shape, the 10-minute throttle, the quota; § Retention: `issue_notifications` pruned at 90 days.
3. Spec 09: § Issues: `channel_ids` on the source create/update/read rows; the new route row `POST /projects/{project}/issues/{issue}/dataset-items` (body, 201 `FromTracesResult`, `ManageDatasets`, errors); § Evaluation & alerts: the condition fields; the MCP tool table gets `list_issues` (arguments, output, `ProjectPerm::View`), "seven" → "eight".
4. Spec 11: issue page buttons ("Add examples to dataset", "Create alert"), the rule form's metric "Issue occurrences" and scope picker, the source editor's Notifications fieldset.
5. `alerts.mdx`: a section "Alert on an issue" (metric, scope, the window rule: `15m` or more, because an occurrence is recorded after its trace completes; and advice: `1h` or more for LLM issues, longer than the refit period), the condition rules in the user's words (no filter, no minimum samples, threshold 0 or more, zero is a value), and the notification digest messages; style per `website/STYLE.md`. `mcp.mdx`: description "seven" → "eight", a table row for `list_issues`, an example question. `issues.mdx`: sections "Notifications", "Create an alert", "Add examples to a dataset". Every code block is tested or marked untested per `check-samples.mjs` (use plain prose and tables where possible).
6. `CHANGELOG.md` § `[Unreleased]` § Added: one line each for the `issue_occurrences` alert condition; issue notifications (channels on issue sources, digests); "Add examples to dataset" from an issue; the `list_issues` MCP tool. Under `### Upgrade notes`: "Migration `0025` rebuilds `notification_deliveries` (an issue notification is a delivery without a rule event) and adds `issue_sources.last_notified_at`." (only the second clause if the first is not user-visible; keep it short).
7. `docs/roadmap.md`: the IC row: M4 "implemented", list the PR when known, decisions D-IC-20 to D-IC-25.

**Tests:** none (the Closing runs `just check-docs`).

**Done when:** by hand, `grep -rn "seven read-only\|four metrics\|Four conditions" docs/spec docs/mvp-decisions.md website/src/content` finds nothing (older plans and the M4 plan keep their words); every new site link exists.

**Commit:** `Document the IC M4 links`

---

## Closing (once, after every phase is committed)

Run in this order, from the worktree, one command at a time (the machine is loaded; do not run them in parallel):
1. Remove every `#[cfg_attr(not(test), allow(dead_code))]` that M4 added (`git diff <base>..HEAD | grep "allow(dead_code)"`), then `just fmt`.
2. `just openapi` (regenerates `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `web/src/mocks/generated/*`, `website/src/generated/openapi.json`). Check by hand that the diff shows exactly: `issue_occurrences` in `AlertMetric`, `issue_id`/`source_id` in `AlertCondition`, `channel_ids` on the three source types, the new `post_issue_dataset_items` operation with `IssueDatasetItemsRequest`.
3. `just api-examples` (regenerates `fixtures/api/static/issue-source*.json` with `channel_ids` and the manifest). If the web tests that read these fixtures need an example of a source with channels, add it to the example generator in `crates/server/src/api_examples.rs` and regenerate (a separate commit step, in the report).
4. `just docs-gen` (expected: no change; M4 adds no CLI flag or SQL view). `just license-fixtures` only if `just check` reports a stale licence fixture (expected: not needed).
5. `just check` (Rust, web, SDKs, docs). Fix failures with Haiku briefs under the same rules; insta snapshots that changed because of the new route, fields or migration (notably `crates/server/tests/snapshots/meta_schema.snap`, from migration `0025`) are reviewed with `cargo insta review`, then accepted, and committed with the generated files.
6. Remove the `#![allow(dead_code)]` of `test_support/issues.rs` (every helper must by now have a caller; delete an unused one). `just assert-clean` after committing the generated files and the accepted snapshots in one commit `Regenerate the IC M4 generated files`.
7. A Sonnet code review of the whole M4 diff against this plan (`git diff <M3 end>..HEAD`), as the user's plan rules say; fixes by Haiku with exact briefs.

Report (the coordinator relays it): what was done; any deviation from this plan, the design or the spec, and why; the answers given to Q1 to Q9; new dependencies (none); anything left unverified.
