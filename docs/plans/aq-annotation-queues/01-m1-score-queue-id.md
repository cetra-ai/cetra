# AQ M1 — `queue_id` on scores

**Summary.** One phase. It adds a nullable `queue_id` (UUID) column to scores, threaded the same way as `dataset_item_id`:
- `cetra-core`'s `Score`, the Arrow `scores` schema and the batch conversion;
- the storage SQL column catalog, and a `queue_id` condition in the storage score list;
- the server's `ScoreCreate`, its write path, its row conversion and `GET /scores?queue_id=`;
- `openapi.json`, `schema.gen.ts` and the API fixtures;
- specs 01, 06 and 09, the generated SQL schema page and the changelog.

The server accepts the field unchecked; M2 adds the check that the queue exists (design § Score column).

**Prerequisites.** The design doc `docs/plans/aq-annotation-queues/00-design.md` (PR #183). Work on branch `claude/aq-impl` in the worktree `/home/sebastien/travaux/llm-observer/.claude/worktrees/aq-annotation-queues-f38be7`.

**Read first.**
- `00-design.md` § Score column (M1) and § Decisions (D-AQ-6).
- `AGENTS.md` § Rules and § Tests.
- `crates/core/src/score.rs` (`Score`), `crates/core/src/schema.rs` (`scores` constants and `scores_schema`) and `crates/core/src/batch.rs` (the scores builder).
- `crates/server/src/api/scores.rs` (`ScoreCreate`, `ScoreListParams`), `crates/server/src/scores.rs` (`write`), `crates/server/src/api/convert.rs` (the score row reader) and `crates/server/src/routes/scores.rs` (the list handler).
- `crates/storage/src/reads/scores.rs` (`ScoreListRequest` and its predicate) and `crates/storage/src/views/catalog.rs` (the SQL column catalog).

## What it is, and what it is not

**What works when it's done:**
- A score written with `"queue_id": "<uuid>"` in `POST /scores` (or `POST /api/v1/scores`) is stored with it.
- `GET /scores?queue_id=<uuid>` returns only those scores.
- The SQL console's `scores` view has a `queue_id` column (VARCHAR).
- Every score read answers `queue_id` when it is set.

**Out of scope:**
- The existence check of the queue, which needs the queues table and comes in M2.
- Any UI.
- The SDKs: the `score()` helpers do not send `queue_id`, and M5's `Client` methods do not either.
- `queue_id` is **not** an input of `derive_id` (D-AQ-6). Do not change `derive_id` or its pinned tests.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AQ-6 | `scores.queue_id`: nullable, set by the writer, not part of the derived id; checked from M2 |
| D185 | No storage format constraint before the first public release: no `FORMAT_VERSION` bump, no WAL version, no golden directory (`fixtures/storage/golden/RELEASES` lists none) |
| D-J-12 | Precedent: `experiment_id`/`dataset_item_id` are request fields of `ScoreCreate` stored on the row |

## Versions

No new dependency.

## Phases

One phase, P1: `openapi.json` embeds core's `Score`, so the core change and the API change cannot be committed apart and keep `just check` green.

---

### P1 — `queue_id` on scores, end to end

**Goal.** `Score` and `ScoreCreate` have `queue_id`; the Arrow schema, batches, the SQL `scores` view, the storage score list, the server write, read conversion and `GET /scores?queue_id=` carry it; OpenAPI, fixtures, fuzz seeds, specs and generated docs follow.

**Edit:**
- `crates/core/src/score.rs`, `crates/core/src/schema.rs`, `crates/core/src/batch.rs`, and the `crates/core/tests/snapshots/*.snap` that change
- `crates/storage/src/views/catalog.rs`, `crates/storage/src/views/mod.rs`, `crates/storage/src/reads/scores.rs`, `crates/storage/src/parquet.rs`
- `crates/server/src/api/scores.rs`, `crates/server/src/scores.rs`, `crates/server/src/api/convert.rs`, `crates/server/src/routes/scores.rs`
- **Every file, anywhere in the workspace** (`crates/core`, `crates/eval`, `crates/storage` including `tests/`, `crates/server`, `crates/cli`, `tools/`), whose `Score { … }`, `ScoreCreate { … }` or `ScoreListRequest { … }` struct literal stops compiling.
  - The compiler lists them; there are about 30 `Score` and 54 `ScoreCreate` literals.
  - In those files, only add the new field next to `dataset_item_id` (or next to `rule_id` for `ScoreListRequest`).
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (generated)
- `fixtures/api/static/schema.json`
- `fuzz/corpus/*/seed-*` files rewritten by `just fuzz-seed` (only those it changes)
- `docs/spec/01-data-model.md`, `docs/spec/05-query.md`, `docs/spec/06-evaluation.md`, `docs/spec/09-api.md`
- the site's SQL schema reference page that `just docs-gen` rewrites (under `website/src/content/docs/docs/`)
- `CHANGELOG.md`

**Steps.**
1. **`Score`.** In `crates/core/src/score.rs`, add this field to `Score` right after `dataset_item_id`:
   ```rust
   /// Annotation queue the score was written from (D-AQ-6). Spec 01 `queue_id`.
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub queue_id: Option<Uuid>,
   ```
   In the file's test sample `Score` (around line 303), set `queue_id: Some("018f6b4e-7c1a-7c1a-8c1a-000000000007".parse().unwrap())`.
2. **Schema.** In `crates/core/src/schema.rs`:
   - add `QUEUE_ID => "queue_id",` to the `scores` names macro, right after `DATASET_ITEM_ID => "dataset_item_id",` (around line 159);
   - in `scores_schema()`, add `field(scores::QUEUE_ID, DataType::Utf8, true),` right after the `DATASET_ITEM_ID` field (around line 374);
   - add `queue_id` at the same position in the column-order test (around line 547).
3. **Batches.** In `crates/core/src/batch.rs`'s scores builder:
   - add a `queue_id: StringBuilder` field after `dataset_item_id`, initialised as `dataset_item_id` is (`string_builder(rows, rows * 36)`);
   - append it with `append_uuid_opt(&mut self.queue_id, score.queue_id)` after `dataset_item_id`;
   - finish it at the schema position;
   - read it back in the batch-to-`Score` conversion the way `dataset_item_id` is read;
   - extend the batch tests (around lines 832, 908 and 962) with one score that has `queue_id` set and one with `None`.
4. **Snapshots.** Run `INSTA_UPDATE=always cargo test -p cetra-core`, then check with `git diff` that the snapshot changes only add `queue_id`.
5. **SQL view.** In `crates/storage/src/views/catalog.rs`, add `col("queue_id", …)` to the scores view's column catalog right after `dataset_item_id`, with the same arguments `dataset_item_id` has apart from the name. In `crates/storage/src/views/mod.rs`, add `queue_id` to the test's expected scores column list at the same position.
6. **Storage list.** In `crates/storage/src/reads/scores.rs`:
   - add this field to `ScoreListRequest<'a>` right after `rule_id`:
     ```rust
     /// Only scores written from this annotation queue (D-AQ-6).
     pub queue_id: Option<&'a str>,
     ```
   - in the predicate builder, next to the `rule_id` conjunct (around lines 266–271), add the same conjunct for `scores::QUEUE_ID`;
   - update the test helper's expected JSON (around line 523) for the new column.
7. **`ScoreCreate`.** In `crates/server/src/api/scores.rs`:
   - add this field to `ScoreCreate` after `dataset_item_id`:
     ```rust
     /// Annotation queue the score is written from (D-AQ-6). Not part of the derived id.
     #[serde(default, skip_serializing_if = "Option::is_none")]
     pub queue_id: Option<Uuid>,
     ```
   - add `queue_id: Option<Uuid>` to `ScoreListParams` next to `rule_id`, documented `/// Only scores written from this annotation queue (D-AQ-6).`
8. **Write.** In `crates/server/src/scores.rs` `write`:
   - destructure `queue_id` from `ScoreCreate` with the other fields (around line 266), and set `queue_id` on the built `Score` (around line 299);
   - do not add it to the `derive_id` call;
   - add no validation (M2 adds it).
9. **Read.** In `crates/server/src/api/convert.rs`, read the `queue_id` column exactly as `dataset_item_id` is read (the column lookup around line 1107, `opt_parsed` around line 1160).
10. **List route.** In `crates/server/src/routes/scores.rs`, bind `let queue_id = params.queue_id.map(|id| id.to_string());` next to the `rule_id` binding (around line 76), and pass `queue_id: queue_id.as_deref()` in the `ScoreListRequest` literal.
11. **Every other literal.** Run `cargo build --workspace --all-targets --all-features`. Add `queue_id: None` to every `Score`, `ScoreCreate` and `ScoreListRequest` literal it reports, next to `dataset_item_id: None` (or `rule_id`). One exception: in `crates/storage/src/parquet.rs`'s round-trip test, set `queue_id` to a fixed `Some(uuid)` (written like the test's other ids), so the round trip covers a set value.
12. **OpenAPI.** Run `just openapi` (regenerates `crates/server/openapi.json` and `web/src/api/schema.gen.ts`).
13. **Fixture schema.** In `fixtures/api/static/schema.json`, add the `queue_id` column to the **first** `dataset_item_id` entry's view (the `scores` view, around line 674; the second one, around line 746, is an experiment view), right after `dataset_item_id`, copying that entry's shape.
14. **Fuzz seeds.** Run `just fuzz-seed` and keep the seed files it rewrites.
15. **Docs.** Run `just docs-gen` (rewrites the site's SQL schema reference).
16. **Specs:**
    - `docs/spec/01-data-model.md` § Score: add the row ``| `queue_id` | Utf8, nullable | annotation queue the score was written from (D-AQ-6); not part of the derived id |`` after `dataset_item_id`.
    - `docs/spec/05-query.md`: if it lists the `scores` view's columns, add `queue_id` after `dataset_item_id`; otherwise leave it.
    - `docs/spec/06-evaluation.md`: at the end of the paragraph that starts "**Deterministic score IDs**" (around line 25), add "`queue_id` (D-AQ-6) is stored as sent and is not an input of the derived id, so a reviewer keeps one score per name and target whichever queue it came from."
    - `docs/spec/09-api.md` § Scores & annotations: append "; `ScoreCreate.queue_id` optional (D-AQ-6)" to the `POST /scores` row (around line 99). Change the `GET /scores` row (around line 102) to "filter by target, name, source, `evaluator_id`, `rule_id`, `queue_id`, time".
17. **Changelog.** In `CHANGELOG.md` § `[Unreleased]` § Added, add "Scores carry an optional `queue_id`, the annotation queue they were written from, and `GET /scores` filters by it."

**Tests:**
- `cetra-core`:
  - the column-order test lists `queue_id`;
  - the batch round trip keeps both `Some(queue_id)` and `None`;
  - the snapshots show the new field.
- `crates/storage/src/reads/scores.rs`: a new test, `list_filters_by_queue_id`, next to the existing `rule_id` filter test.
  - Store three scores: one in queue A, one in queue B, one with no queue.
  - `queue_id = Some(A)` returns exactly the first.
- `crates/storage/src/parquet.rs`: the round trip with `queue_id` set.
- `crates/storage/src/views/mod.rs`: the column list includes `queue_id`.
- `crates/server/src/routes/scores.rs`: a new test, `scores_store_and_filter_by_queue_id`, next to the existing `rule_id` filter test (around line 310).
  - `POST /scores` writes two human scores on two traces, one with `queue_id` Q and one without.
  - `GET /scores?queue_id=Q` returns exactly the first, and its JSON has `"queue_id": "<Q>"`.
  - `GET /scores` returns both; the second has no `queue_id` key.
- `crates/server/src/routes/scores.rs`: a new test, `queue_id_does_not_change_the_derived_id`.
  - Write the same human score twice, with the same target and name and no `score_id`: first without `queue_id`, then with Q.
  - Both answers return the same `score_id`.
  - `GET /scores` shows one live score, with `queue_id` Q.
- Unchanged and passing:
  - `derive_id`'s pinned tests;
  - the `api_examples` tests;
  - `contract_file_is_up_to_date` (`crates/server/tests/openapi.rs`);
  - `sql_schema_reference_is_up_to_date`;
  - the fuzz corpus replay (`cargo test -p cetra-storage fuzz_entry`).

**Done when.**
- `cargo fmt --all --check`
- `just lint`
- `cargo test -p cetra-core -p cetra-storage`
- `cargo test -p cetra-server`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` passes once, right before the commit. Node comes from nvm.
- `git status` shows only files allowed by the Edit list.

**Commit:** `Add queue_id to scores`
