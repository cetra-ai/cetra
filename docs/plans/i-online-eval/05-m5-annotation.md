# I · M5 — Annotation: the annotation form and score ownership

Implementation plan for milestone M5 of track I. It has 4 sub-phases, I5.1 to I5.4. **Haiku** implements each one in one go, with no context beyond this plan and the files it names. Sub-phases that do not depend on each other run in parallel worktrees (§ Parallel waves). Each sub-phase ends with `just check` green and one commit. Sonnet reviews the code after wave 1 (I5.1, I5.2) and after waves 2–3 (I5.3, I5.4).

**Prerequisites** (check them before you start):
- M1 to M4 are merged on `main`: `crates/server/src/meta/score_definitions.rs`, `crates/server/src/routes/score_definitions.rs`, `web/src/routes/$org/$project/settings/score-definitions.tsx` and `web/src/components/trace/run-evaluator-dialog.tsx` exist, and `crate::scores::write` checks score definitions (`check_definition` in `crates/server/src/scores.rs`). Start from `main`.
- Check that this plan file, `docs/plans/i-online-eval/05-m5-annotation.md`, exists.
- D-I-36 and D-I-37 are in `docs/plans/i-online-eval/00-design.md` and `docs/mvp-decisions.md` § Track I, and D-I-26's ownership bullet points at D-I-36 (the plans PR adds them). If they are not, STOP and report it.
- Node `26.13.0` is installed at `$HOME/.local/share/nvm/v26.13.0/bin`. It is not on the non-interactive `PATH` (M1 § Conventions).

**Read first** (every sub-phase):
- `AGENTS.md`, all of it.
- This file's sections up to § I5.1, then your own sub-phase.
- `docs/plans/i-online-eval/00-design.md` § Decisions: D-I-18, D-I-24, D-I-26, D-I-27, D-I-35.
- `docs/plans/i-online-eval/03-m3-queue-rules.md` § Conventions for every sub-phase. M3's and M4's patterns (routes, tests, fixtures, pages) are the model for M5.
- `docs/spec/06-evaluation.md` § Scores; `docs/spec/08-auth.md` § Authorization (RBAC); `docs/spec/09-api.md` § Scores & annotations; `docs/spec/11-ui.md` § Trace view and § Sessions.
- The code each sub-phase points to, before you edit it.

## What M5 is, and what it is not

When M5 is done:
- **Score ownership** (server). A `human` score belongs to its `author_user_id`. A score write through a web session or a personal access token (PAT) whose `score_id` names the live `human` score of another user, and a `DELETE` of such a score, answer `403 forbidden`, unless the caller is a project admin. `sk` keys, public feedback and evaluators keep G's rules (D-G-27).
- **The annotation form** (web). It is built from the project's live score definitions, one control per definition, and edits only the caller's own `human` scores. It writes through G's route without `score_id`, so the server derives one score per annotator (D-G-28).
  - In the trace view (on the traces list split, the full trace page and the session page's split), the span detail gets an **Annotate** tab, with a Trace/Span switch.
  - On the session page, the header gets an **Annotate** button that opens the form on the session in a side sheet.

**Out of scope:**
- annotation queues;
- a UI for admins to change or delete other people's scores (they can through the API);
- annotating a score name that has no live definition;
- backfill and the M6 end-to-end test;
- any change to `crates/core`, `crates/storage` or the score write contract (`ScoreCreate`, `ScoreBatch`, `ScoreWriteResult`).

## Decisions this plan relies on

| # | Summary |
|---|---|
| D-G-16 | Project score writes need role `member`; a key needs scope `write`. |
| D-G-27 | The caller decides `source` and the author. A session stores `human` with `author_user_id`; a PAT or `sk` key on the project route stores `api` (or `sdk` when the body says so). |
| D-G-28 | A missing `score_id` is derived (UUIDv8) from the project, target, name, source and author, so re-annotating replaces your own score and two annotators keep two rows. |
| D-I-18 | Evaluator scores copy `session_id` and `user_id` from the target trace. The annotation form does the same (D-I-37). |
| D-I-24 | `ManageEvaluation` (member) covers score definitions; reads use `View`. |
| D-I-26 | Score definitions; score writes checked against the live definition of their name; the annotation panel on trace, span and session; the ownership rule. |
| D-I-27 | Score definitions are managed at `/$org/$project/settings/score-definitions`. |
| D-P-17 | At the monthly units cap, score writes answer `402 quota_exceeded`; deletions are never refused. The form shows that answer. |

**This plan adds two decisions** (`D-I-36`, `D-I-37`). They are already in `docs/mvp-decisions.md` § Track I and `00-design.md` § Decisions (see this plan's § Prerequisites); I5.4 only checks they are still there.

| # | Decision |
|---|---|
| D-I-36 | **How ownership is checked.** `scores::Writer` names the person behind a write: `Human { user_id, admin }` (a session) and a new `Pat { token_id, user_id, admin }` (a PAT, which still stores `source = api` with the token as the derived-id author, as D-G-27 says). `admin` is "the caller's effective project role is `admin`" (`ProjectAccess.role == Some(Role::Admin)`; org owners and admins are project admins). `Api { author }` is now only an `sk` key on the project route.<br>- `scores::write` checks after its validation (a `400` comes before a `403`) and after the units cap (`402` first). For a person, its by-id lookup covers every id of the batch, UUIDv7 included; for other writers it stays as today (non-v7 ids only). Every looked-up live score that is `human` and whose `author_user_id` is not the caller refuses the whole batch with `403 forbidden`, unless the caller is an admin. One lookup serves both the times and the check.<br>- `scores::delete` takes the `Writer` and makes the same check on the score it found.<br>- An admin who replaces another user's `human` score becomes its author (the stored row has the admin's `author_user_id`). A PAT that replaces its own user's `human` score stores `api`, as D-G-27 says.<br>- A write without `score_id` cannot hit another user's score: the derived id includes the author. So the form never meets a `403` in normal use.<br>- A person's write that supplies UUIDv7 ids now makes one storage lookup it did not make before, so it can answer `503` when storage cannot answer, as non-v7 ids already could.<br>- The check and the append are not atomic: two concurrent writes can both pass the check. This race is accepted; G's score path has no per-id lock.<br>- A person may replace another user's non-`human` score by id (G's rules); the stored version is then their own `human` score. |
| D-I-37 | **The annotation form.**<br>- It lists every live definition of the project (`GET /score-definitions?include_archived=false&limit=1000`; 1 000 is the project cap), sorted by name. Each definition has one control: numeric → a number field (with the definition's range), boolean → Yes/No radios, categorical → one radio per category, text → a text area. Each also has an optional one-line reason.<br>- It shows and edits only the caller's own scores: the live `human` scores on the target whose `author_user_id` is the caller (`GET /api/auth/me`). Other people's scores stay in the read-only **Scores** tab.<br>- **Save** sends the changed rows in one `POST /scores`, without `score_id` and without `source`. A categorical value carries its category's numeric `value` when the definition has one. A trace or span annotation carries the trace's `session_id` and `user_id`; a session annotation carries the session's `user_id` (as evaluator scores do, D-I-18).<br>- **Clear** deletes the caller's own score on that row (`DELETE /scores/{id}`). After a save or a clear, the form refetches the score reads (score lists, trace and session lists and details).<br>- In the trace view, a Trace/Span switch picks the target; the trace is the default.<br>- Viewers see "Only members can annotate." in place of the form (`ProjectPerm::Annotate` is `member`, D-G-16). With no live definition, the form says so and links members to the score definitions page. |

## Versions

No new dependency, in Rust or in the web.

## Conventions for every sub-phase

M3's conventions apply unchanged (`03-m3-queue-rules.md` § Conventions for every sub-phase, which includes M1's and M2's):
- the Node `PATH` and the `just` recipes;
- contract regeneration (`just openapi`, `just api-examples`);
- logs of a check run under your worktree's own `target/`;
- `just check-rust` as one background task;
- the clippy denies;
- `tests/openapi.rs`, which pins every operation;
- no `unwrap`/`expect` outside tests;
- privacy in logs;
- STOP rules;
- one commit per sub-phase with no trailer lines;
- the report.

In addition:
- **No dead code.** M5 adds no `#[allow(dead_code)]`. Every new Rust item has a caller in the same sub-phase. If `just check` reports an unused item, STOP and report it.
- **Web tests** follow `web/src/components/trace/scores-tab.test.tsx` and `web/src/components/trace/run-evaluator-dialog.test.tsx`:
  - pages are mounted with `renderApp(url)` (`web/src/test/render.tsx`);
  - every endpoint a test needs beyond the manifest mocks is overridden with `server.use(http.…)`;
  - a write is captured by a `server.use` handler that pushes `await request.json()` into an array;
  - Radix tabs are switched with `fireEvent.mouseDown(tab)`, as `scores-tab.test.tsx` does (a plain click does not switch them in jsdom);
  - radios (`role="radio"`) and buttons are clicked with `user.click`;
  - number fields are `role="spinbutton"`: `await user.clear(field); await user.type(field, "0.8")`.
- **`web/src/test/setup.ts` fails a test that made an unmocked request.** `POST` and `DELETE …/scores` have no test mock: every test that clicks Save or Clear installs its own handler. The dev-server mocks of I5.2 (`web/src/mocks/writes.ts`) are never installed in tests.
- **The form never sends `score_id` or `source`.** The server derives the id per annotator (D-G-28) and stores `human` for a session (D-G-27).
- **Privacy.** The web never logs score values or reasons. The server logs no score content (ids and counts only), as today.

## Parallel waves

Each wave starts when the waves before it are merged into the M5 integration branch. The sub-phases in one wave touch different files.

| Wave | Sub-phases | Depends on | Shared files to merge by hand |
|---|---|---|---|
| 1 | I5.1 server: score ownership · I5.2 web: annotation logic, queries, role, mocks | M4 on `main` | none (I5.1 regenerates `crates/server/openapi.json` and `web/src/api/schema.gen.ts`; I5.2 touches neither) |
| 2 | I5.3 web: the annotation form and the trace view's Annotate tab | I5.2 | — |
| 3 | I5.4 web: the session sheet; specs and roadmap | I5.1, I5.3 | — |

The Sonnet code reviews fit the waves: after wave 1 (I5.1, I5.2), and after waves 2–3 (I5.3, I5.4).

After merging wave 1, run `just openapi` once and commit any diff as `Regenerate the API contract` (normally none).

## Final layout after M5

```
crates/server/src/scores.rs                 Writer::Human { admin }, Writer::Pat, Writer::person,
                                            check_owner, the check in write and delete (I5.1)
crates/server/src/routes/scores.rs          project_writer; delete takes the writer (I5.1)
crates/server/src/api/openapi.rs            create_scores / delete_score descriptions (I5.1)
crates/server/openapi.json, web/src/api/schema.gen.ts   regenerated (I5.1)
web/src/lib/annotation.ts (+ .test.ts)      targets, drafts, own scores, items, row errors,
                                            score-read invalidation (I5.2)
web/src/lib/roles.ts (+ .test.ts)           canAnnotate (I5.2)
web/src/api/queries.ts                      scoresQuery takes session_id (I5.2)
web/src/api/queries-evaluation.ts           liveScoreDefinitionsQuery (I5.2)
web/src/mocks/writes.ts (+ .test.ts)        create_scores, delete_score dev answers (I5.2)
web/src/test/score-definitions.ts           ANNOTATION_DEFINITIONS, serveAnnotationDefinitions (I5.2)
web/src/components/annotation/annotation-form.tsx      AnnotationForm (I5.3)
web/src/components/trace/annotate-tab.tsx (+ .test.tsx)   AnnotateTab (I5.3)
web/src/components/trace/span-detail.tsx (+ .test.tsx)    the Annotate tab (I5.3)
web/src/components/session/session-view.tsx            Annotate button and sheet (I5.4)
web/src/routes/$org/$project/sessions/$sessionId.test.tsx   new cases (I5.4)
docs/spec/06-evaluation.md, 08-auth.md, 09-api.md (I5.1); 11-ui.md, roadmap (I5.4)
```

## Shared reference

### Fixed identifiers (fixtures and tests)

The example project's ids are in `fixtures/api/README.md` § Fixed identifiers. The ones M5's tests use:

| Thing | Value |
|---|---|
| Ada (`me.json`, org owner, so project admin) | `01900000-0000-7000-8000-000000000100` (`ADA_ID` in `web/src/test/fixtures.ts`) |
| Bob (org member) | `01900000-0000-7000-8000-000000000101` (`BOB_ID`) |
| Trace 1 | `a1000000000000000000000000000001`, session `sess-001`, user `user-42` |
| Span 4 of trace 1 | `b100000000000014` |
| Session | `sess-001`, user `user-42`; its fixture score `…0305` is `resolved = true`, `human`, by Ada |
| Test definitions (web, I5.2) | `…0601` `helpfulness` numeric 0–1; `…0602` `resolved` boolean; `…0603` `tone` categorical `friendly` (1), `neutral` (0.5), `rude` (0); `…0604` `note` text. The prefix `…` is `01900000-0000-7000-8000-000000000` |
| Ada's own score in web tests | `01900000-0000-7000-8000-000000000310` (`helpfulness = 0.7` on trace 1) |
| Bob's score in web tests | `01900000-0000-7000-8000-000000000311` (`helpfulness = 0.2` on trace 1) |
| Server test score ids (UUIDv7) | `01900000-0000-7000-8000-000000000410`, `…0411`, `…0412` |

### Who may replace or delete a live score (after I5.1)

| Caller (project route) | `Writer` | Another user's `human` score | Any other score |
|---|---|---|---|
| session, project admin | `Human { admin: true }` | allowed; the admin becomes its author | allowed (G) |
| session, member | `Human { admin: false }` | `403 forbidden` | allowed (G) |
| PAT with `write`, its user a project admin | `Pat { admin: true }` | allowed; stored as `api` (G) | allowed (G) |
| PAT with `write`, its user a member | `Pat { admin: false }` | `403 forbidden` | allowed (G) |
| `sk` key with `write` | `Api` | allowed (G, accepted in D-I-26) | allowed (G) |
| `sk` key on `POST /api/v1/scores`, feedback, evaluators | `Sdk`, `EndUser`, `Evaluator` | allowed (G) | allowed (G) |

"Own" means the live score's `author_user_id` is the caller's user id. The check runs only on ids that already have a live version; a new id is never refused.

---

## I5.1 — Server: score ownership

**Goal:** the ownership rule of D-I-26, as D-I-36 shapes it, in `scores::write` and `scores::delete`. The score routes build the writer from the caller and the caller's project role.

**Edit only:**
- `crates/server/src/scores.rs`;
- `crates/server/src/routes/scores.rs`;
- `crates/server/src/api/openapi.rs` (two description strings);
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (regenerated by `just openapi`);
- `docs/spec/06-evaluation.md`, `docs/spec/08-auth.md`, `docs/spec/09-api.md`.

**Commit:** `Refuse changes to another user's human score`

### `scores.rs`: the writer

Change `Writer` to:

```rust
/// Who is writing, which decides `source`, the author and the ownership check (D-G-27, D-I-36).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Writer {
    /// A web session on the project route. `admin`: the caller's effective project role is admin.
    Human { user_id: Uuid, admin: bool },
    /// A personal access token on the project route: the token, its user, and whether that user
    /// is a project admin. Its scores are stored as `api`, like a key's (D-G-27).
    Pat {
        token_id: Uuid,
        user_id: Uuid,
        admin: bool,
    },
    /// An `sk` key (`key_id`) on the project route.
    Api { author: Uuid },
    // `Sdk`, `EndUser` and `Evaluator` are unchanged.
}
```

In `impl Writer`:
- `source`: a PAT follows the `Api` arms. Write them as `(Self::Api { .. } | Self::Pat { .. }, None | Some(ScoreSource::Api)) => Some(ScoreSource::Api)` and `(Self::Api { .. } | Self::Pat { .. }, Some(ScoreSource::Sdk)) => Some(ScoreSource::Sdk)`. `Self::Human { .. }` patterns are unchanged.
- `author`: `Self::Human { user_id, .. } => user_id`, `Self::Pat { token_id, .. } => token_id`. The derived ids of PAT writes therefore stay what they were (the token id), and `derive_id_matches_the_pinned_value` is unchanged.
- `author_user`: `Self::Human { user_id, .. } => Some(user_id)`; `Self::Pat { .. }` joins the `None` arm.
- `evaluator_ids`: `Self::Pat { .. }` joins the `(None, None)` arm.
- New:

```rust
    /// The person behind a session or PAT write, and whether they are a project admin. `None`
    /// for keys, feedback and evaluators, which the ownership check leaves alone (D-I-26).
    fn person(self) -> Option<(Uuid, bool)> {
        match self {
            Self::Human { user_id, admin } | Self::Pat { user_id, admin, .. } => {
                Some((user_id, admin))
            }
            Self::Api { .. } | Self::Sdk { .. } | Self::EndUser { .. } | Self::Evaluator { .. } => {
                None
            }
        }
    }
```

Next to `check_definition`, add:

```rust
/// `403 forbidden` when a person who is not a project admin would replace or delete another
/// user's `human` score (D-I-26, D-I-36). Keys, feedback and evaluators always pass.
fn check_owner(writer: Writer, live: &Score) -> Result<(), ApiError> {
    let Some((user_id, admin)) = writer.person() else {
        return Ok(());
    };
    if admin || live.source != ScoreSource::Human || live.author_user_id == Some(user_id) {
        return Ok(());
    }
    Err(ApiError::Forbidden {
        code: ProblemCode::Forbidden,
    })
}
```

Import `ProblemCode` from `crate::api::common` (production code now uses it; the tests' own `use crate::api::common::ProblemCode;` then becomes redundant: remove it if clippy says so).

### `scores.rs`: the check in `write`

Replace the `lookup` block (today: the non-v7 ids of `kept`) with:

```rust
    // A person's write also looks up the UUIDv7 ids it supplies, for the ownership check
    // (D-I-36). Other writers need only the non-v7 ids, for the times.
    let person = writer.person().is_some();
    let lookup: Vec<String> = kept
        .iter()
        .filter(|pending| person || !is_v7(pending.id))
        .map(|pending| pending.id.to_string())
        .collect();
    let live = live_scores(state, project, &lookup).await?;
    for pending in &kept {
        if let Some(found) = live.get(&pending.id) {
            check_owner(writer, found)?;
        }
    }
```

Nothing else in `write` changes. `times` already ignores `live` for a UUIDv7 id. The order of checks is: units cap (`402`), validation (`400`), lookup (`503` when storage cannot answer), ownership (`403`), append.

### `scores.rs`: `delete`

```rust
/// Appends a deleted copy of the live score `score_id` (spec 03 §5). `NotFound` when none;
/// `Forbidden` when `writer` may not delete it (D-I-36).
pub(crate) async fn delete(
    state: &AppState,
    project: &ProjectRow,
    writer: Writer,
    score_id: Uuid,
) -> Result<(), ApiError>
```

Right after the `let Some(mut tombstone) = live.remove(&score_id) else { … };` line, add `check_owner(writer, &tombstone)?;`. Update the module doc's first lines if they describe the callers.

### `routes/scores.rs`

Import `Role` with the other roles (`use crate::auth::roles::{KeyScope, ProjectPerm, Role};`). Add:

```rust
/// The writer of a project score route and its log label (D-G-27). Sessions and PATs carry
/// their user and whether that user is a project admin, for the ownership check (D-I-36).
fn project_writer(
    principal: &Principal,
    access: &ProjectAccess,
) -> Result<(Writer, &'static str), ApiError> {
    let admin = access.role == Some(Role::Admin);
    match principal {
        Principal::Session { user, .. } => Ok((
            Writer::Human {
                user_id: user.id,
                admin,
            },
            "human",
        )),
        Principal::Pat { user, token_id, .. } => Ok((
            Writer::Pat {
                token_id: *token_id,
                user_id: user.id,
                admin,
            },
            "api",
        )),
        Principal::ApiKey { key_id, .. } => Ok((Writer::Api { author: *key_id }, "api")),
        Principal::ControlPlane { .. } => Err(ApiError::Forbidden {
            code: ProblemCode::Forbidden,
        }),
    }
}
```

- `create_scores` replaces its `match principal { … }` with `let (writer, kind) = project_writer(&principal, &access)?;`.
- `delete_score` gains `Extension(principal): Extension<Principal>` (as `create_scores` takes it), builds `let (writer, _) = project_writer(&principal, &access)?;`, and calls `score_writes::delete(&state, &access.project, writer, path.score_id)`.
- Update the doc comments of both handlers to mention the ownership check (D-I-36).

`ProjectAccess.role` is `Some` for sessions and PATs (`http/access.rs` `user_project`) and `None` for keys, so a key is never an admin here; it does not need to be.

Other callers of `scores::write` (`routes/feedback.rs` with `Writer::EndUser`, `eval/execute.rs` with `Writer::Evaluator`) do not change.

### OpenAPI

In `crates/server/src/api/openapi.rs`, change only the descriptions of `operation("create_scores", …)` and `operation("delete_score", …)`. Do not touch `operation("create_key_scores", …)`: its description starts with the same sentence, so find the right one by its operation id, not by its text.
- `create_scores` description becomes: "Writes 1–1000 scores in one batch. The answer has one id per item, in input order. With a session or a personal access token, a `score_id` that names another user's `human` score is refused with 403 unless the caller is a project admin."
- `delete_score` description becomes: "Appends a deleted version of the score. Its `created_at` is kept. With a session or a personal access token, another user's `human` score can be deleted only by a project admin (403)."

Their status lists already have 403. `crates/server/tests/openapi.rs` needs no change. Run `just openapi` and commit both generated files.

### Spec

- `docs/spec/06-evaluation.md` § Scores, after the paragraph that starts "The caller decides `source` (D-G-27)", add: "**Ownership** (D-I-26, D-I-36). A `human` score belongs to its `author_user_id`. Through a session or a personal access token, a write whose `score_id` names the live `human` score of another user, or a `DELETE` of one, is `403 forbidden` unless the caller is a project admin. Validation errors (`400`) and the units cap (`402`) come first. An admin who replaces another user's score becomes its author. A member may replace another user's non-`human` score (`api`, `sdk`, `end_user` or an evaluator's) by its id, deliberately: the stored version becomes their own `human` score (D-I-26). `sk` keys, public feedback and evaluators keep the rules above: they can replace a `human` score whose id they know. A person's write without `score_id` derives the id from their own user, so it never replaces anyone else's score. A person's write that supplies UUIDv7 ids looks them up in storage first, so it can answer `503` when storage is unavailable. The check and the append are not atomic; a concurrent race is accepted."
- `docs/spec/08-auth.md` § Authorization (RBAC), after the sentence "Evaluators, online rules and score definitions: project `member` and above; API keys cannot manage them (D-I-24).", add: "Annotations: a member changes or deletes only their own `human` scores; a project admin may change or delete anyone's (D-I-26, D-I-36). API keys keep their scope rules."
- `docs/spec/09-api.md` § Scores & annotations:
  - in the `POST /scores` row, after "Role `member`, key scope `write`", add "; through a session or a PAT, replacing another user's `human` score needs project admin (403, D-I-36)";
  - in the `DELETE /scores/{score_id}` row, after "`204`, `404` when there is none", add "; `403` for another user's `human` score unless a project admin (session or PAT, D-I-36)".

### Tests

**The test clock.** `TestApp` runs on a manual clock that does not move by itself. Two versions of one UUIDv7 id written at the same instant get the same `updated_at` and `ingest_time`, so which one is live is arbitrary. A version written right after a delete is older than the tombstone, because the tombstone takes `now.max(previous updated_at + 1)`. So, in every test below, **before every write or delete of an id that already has a stored version (including a write after a delete), call `app.state.clock.advance(std::time::Duration::from_secs(60));`**, as `update_keeps_created_at` in `scores.rs` does. A write or delete that the test expects to be refused (`403`, `400`) needs no advance, but the next accepted one does. The steps below write "advance" where it goes; do not leave one out.

In `scores.rs` tests:
- Update the existing callers. `sources_follow_the_caller` builds `Writer::Human { user_id: user, admin: false }`. `delete_hides_the_score_and_a_second_delete_is_not_found` calls `delete(&app.state, &seeded.project, api_writer(), id)` twice.
- Extend `sources_follow_the_caller`: a `Writer::Pat { token_id: author(), user_id: user, admin: false }` write of `item("clarity", 0.5)` with no source is stored (find the row by `name == "clarity"`) with `source == ScoreSource::Api` and `author_user_id == None`.
- Shared setup for the tests below. Users A and B are `Uuid::from_u128` values, as `sources_follow_the_caller` builds its user. "A's human score" means: writer `Human { user_id: A, admin: false }` writes `item("tone", 1.0)` with `score_id: Some(V7_FIXED.parse().unwrap())` (the module's fixed UUIDv7; call it `ID`). A second UUIDv7 is `v7_at(1_717_986_918_401)` (call it `NEW`).
- `write_refuses_another_users_human_score_to_a_person`:
  - A's human score.
  - `Human { user_id: B, admin: false }` writes `ID` with value 0.0 → `Err(ApiError::Forbidden { code: ProblemCode::Forbidden })`. The stored row still has value 1.0 and `author_user_id == Some(A)`.
  - `Pat { token_id: <any>, user_id: B, admin: false }` writes `ID` → `Forbidden`.
  - Advance. A writes `ID` with value 0.5 → `Ok`; the stored row has 0.5.
  - Advance. `Human { user_id: B, admin: true }` writes `ID` with value 0.0 → `Ok`; the stored row has value 0.0 and `author_user_id == Some(B)`.
- `a_refused_batch_stores_nothing`: A's human score. `Human { user_id: B, admin: false }` writes the batch `[item("tone", 0.3) with score_id NEW, item("tone", 0.0) with score_id ID]` → `Forbidden`. `stored(…)` has exactly one row (`ID`, value 1.0, author A), and no row with id `NEW`.
- `write_checks_ownership_after_validation`: A's human score. B (`admin: false`) writes the batch `[item("tone", 0.0) with score_id ID, item("", 1.0)]` → `ApiError::BadRequest` (assert with `refused`), not `Forbidden`.
- `keys_and_evaluators_skip_the_ownership_check`:
  - A's human score.
  - Advance. `api_writer()` writes `ID` with value 0.25 → `Ok`; the stored row has `source == ScoreSource::Api`.
  - Advance. A writes `ID` again (value 1.0) → `Ok`, a `human` score of A again.
  - Advance. `Writer::Evaluator { kind: EvaluatorKind::Check, evaluator_id: <any>, rule_id: None }` writes `ID` with name `tone` → `Ok`. Read `evaluator_writer_sets_source_ids_and_author` first, and reuse its setup if the evaluator needs to exist.
- `a_person_may_replace_another_users_non_human_score`: `api_writer()` writes `item("tone", 1.0)` with `score_id: Some(v4(7))`. Advance. `Human { user_id: B, admin: false }` writes `v4(7)` with value 0.0 → `Ok`; the stored row is `human` with `author_user_id == Some(B)` and value 0.0.
- `a_person_may_delete_a_non_human_score`:
  - `api_writer()` writes `v4(8)`. Advance. `delete` with `Human { user_id: B, admin: false }` → `Ok`, and the score is gone.
  - An evaluator writer (as in `keys_and_evaluators_skip_the_ownership_check`) writes `v4(9)`. Advance. `delete` with `Human { user_id: B, admin: false }` → `Ok`.
- `delete_refuses_another_users_human_score_to_a_person`:
  - A's human score.
  - `delete` with `Human { user_id: B, admin: false }` → `Forbidden`, and the score is still stored.
  - `delete` with `Pat { token_id: <any>, user_id: B, admin: false }` → `Forbidden`.
  - Advance. `delete` with `api_writer()` → `Ok`.
  - Advance. A writes `ID` again → `Ok`, and it is stored (live) again. Advance. `delete` with `Human { user_id: B, admin: true }` → `Ok`.
  - Advance. A writes `ID` again → `Ok`. Advance. `delete` as A (`Human { user_id: A, admin: false }`) → `Ok`, and nothing is stored.

In `routes/scores.rs` tests, over HTTP:
- Setup. Seed with `let seeded = app.seed().await;`. Ada is the org owner, hence project admin, and her session is `seeded.cookie`. Bob is `let bob = app.add_member(&seeded, "bob@example.com", OrgRole::Member).await;` (with `use crate::auth::roles::OrgRole;`), and his session is sent as `.header("cookie", &bob.cookie)`. PATs are `let pat = app.personal_token(bob.id, true).await;` (or `seeded.user_id` for Ada's), sent as `.header("authorization", &format!("Bearer {pat}"))`. Check these signatures in `crates/server/src/test_support/mod.rs` before you use them.
- A score body is `ScoreBatch { scores: vec![ScoreCreate { score_id: Some("<id>".parse().unwrap()), ..span_score("tone", <value>) }] }`. The ids are `01900000-0000-7000-8000-000000000410` to `…0413`.
- "Advance" means `app.state.clock.advance(std::time::Duration::from_secs(60));`, by the rule above.
- `a_member_cannot_replace_or_delete_another_users_human_score`:
  - Ada `POST`s `…0410` with value 1.0 → 200.
  - Bob `POST`s `…0410` with value 0.0 → 403, problem `code == "forbidden"`.
  - Bob `DELETE`s it → 403.
  - `listed` still has one row, value `Numeric(1.0)`, `author_user_id == Some(seeded.user_id)`.
- `a_refused_batch_writes_nothing`: Ada writes `…0410`. Bob `POST`s one batch with `…0413` (new) and `…0410` → 403. `listed` has only `…0410`.
- `a_project_admin_can_replace_and_delete_another_users_human_score`:
  - Bob writes `…0411` with 1.0 → 200.
  - Advance. Ada writes `…0411` with 0.0 → 200; the listed row has `author_user_id == Some(seeded.user_id)` and value 0.0.
  - Bob writes `…0411` again → 403, because it is Ada's now.
  - Advance. Ada deletes it → 204.
- `a_member_replaces_and_deletes_their_own_human_score`: Bob writes `…0412` with 1.0 → 200. Advance. Bob writes it with 0.5 → 200, and the listed value is 0.5. Advance. Bob deletes it → 204.
- `a_personal_token_is_checked_as_its_user`:
  - Ada writes `…0410`.
  - Bob's PAT writes it → 403; Bob's PAT deletes it → 403.
  - Advance. Ada's PAT writes it → 200; the listed row has `source == ScoreSource::Api` (G's rule for PATs).
- `a_write_key_keeps_g_rules_over_human_scores`:
  - Create a key: Ada `POST`s `/api/v1/projects/{seeded.project.id}/api-keys` with her cookie (`seeded.cookie`) and `{"kind": "secret", "scopes": ["write"]}` → 201; read `secret` from the JSON (`routes/api_keys.rs` tests do the same).
  - Bob writes `…0411`.
  - Advance. The key writes it → 200, with `source == Api`.
  - Advance. The key deletes it → 204.
- `a_member_may_delete_a_non_human_score`: its own `TestApp`, seed and Bob; create a write key exactly as in `a_write_key_keeps_g_rules_over_human_scores` (factor the creation into a small test helper `write_key(&app, &seeded) -> String` used by both). The key writes `…0412` → 200. Advance. Bob deletes it → 204, and `listed` is empty.
- `ownership_needs_an_existing_score`: Bob writes a new UUIDv7 id `…0413` → 200, with no prior score (the lookup finds nothing).

The test `delete_hides_the_score_and_refuses_unknown_ids` keeps passing unchanged: Ada deletes her own score.

### Done when

`just check` passes. `rg -n "Writer::Api \{ author: token_id" crates/server` is empty (a PAT is never `Api`).

---

## I5.2 — Web: annotation logic, queries, the role check and the dev mocks

**Goal:** everything the form needs that is not a component: the target and draft types, the pure functions that turn scores into form values and form values into a `ScoreCreate`, the query options, `canAnnotate`, the score-read invalidation, the dev-server answers of the two score writes, and a test helper that serves annotation definitions.

**Edit only:**
- `web/src/lib/annotation.ts` and `.test.ts` (new);
- `web/src/lib/roles.ts` and `.test.ts`;
- `web/src/api/queries.ts` (`scoresQuery` only);
- `web/src/api/queries-evaluation.ts`;
- `web/src/mocks/writes.ts` and `.test.ts`;
- `web/src/test/score-definitions.ts` (new).

**Commit:** `Add the annotation form's logic and queries`

### `api/queries.ts`

`scoresQuery`'s `query` type gains `session_id?: string` (the route already accepts it, `ScoreListParams.session_id`). Nothing else changes, so the Scores tab keeps its query keys.

### `api/queries-evaluation.ts`

```ts
/** Every live score definition of the project, for the annotation form (D-I-37). 1 000 is the project cap. */
export function liveScoreDefinitionsQuery(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/score-definitions", {
    params: {
      path: { project: projectRef(org, project) },
      query: { limit: 1000, include_archived: false },
    },
  });
}
```

`scoreDefinitionsKey` (a prefix of this key) already invalidates it when a definition is created, edited or archived.

### `lib/roles.ts`

```ts
/** Mirrors `ProjectPerm::Annotate`: project members and above write scores (D-G-16). Same logic as `canManageEvaluation`, kept separate on purpose: it mirrors a different server permission. */
export function canAnnotate(role: OrgRole | undefined): boolean {
  return atLeast(role, "member");
}
```

`roles.test.ts`: `canAnnotate` is false for `undefined` and `"viewer"`, and true for `"member"`, `"admin"` and `"owner"`.

### `lib/annotation.ts`

```ts
import type { QueryClient } from "@tanstack/react-query";
import { problemOf } from "@/api/problem";
import { scoresQuery } from "@/api/queries";
import type { Schema } from "@/api/types";

type Score = Schema<"Score">;
type ScoreDefinition = Schema<"ScoreDefinition">;

/** What an annotation is attached to. */
export type AnnotationTarget =
  | { type: "trace"; traceId: string }
  | { type: "span"; traceId: string; spanId: string }
  | { type: "session"; sessionId: string };

/** Ids copied onto the score, as evaluator scores copy them (D-I-18, D-I-37). */
export type AnnotationContext = { sessionId?: string; userId?: string };

/** One row of the form as typed. `value` is "" when unset; see `draftOf` for each type's text. */
export type Draft = { value: string; reason: string };

export const EMPTY_DRAFT: Draft = { value: "", reason: "" };
```

Functions, all exported, each with a one-line doc comment:

| Function | Behaviour |
|---|---|
| `targetKey(target: AnnotationTarget): string` | `trace:<traceId>`, `span:<traceId>:<spanId>`, `session:<sessionId>`. The form is keyed by it, so a new target starts from a clean form. |
| `scoreTarget(target: AnnotationTarget): Schema<"ScoreTarget">` | `{type: "trace", trace_id}`, `{type: "span", trace_id, span_id}` or `{type: "session", session_id}`. |
| `scoreListQuery(org, project, target)` | `scoresQuery(org, project, …)` with `{trace_id, target: "trace", limit: 1000}`, `{trace_id, span_id, target: "span", limit: 1000}` or `{session_id, target: "session", limit: 1000}`. The trace and span forms are exactly the Scores tab's queries (`components/trace/scores-tab.tsx`), so both share the cache. |
| `onTarget(score: Score, target: AnnotationTarget): boolean` | The score's `target` is the same kind with the same ids. |
| `ownScores(scores: readonly Score[], target: AnnotationTarget, userId: string): Map<string, Score>` | By score name: the scores that are not `deleted`, are `source === "human"`, have `author_user_id === userId`, and are `onTarget`. When two share a name, the greater `updated_at` wins (compare the RFC 3339 strings, as `scores-tab.tsx` does). |
| `draftOf(score: Score \| undefined, definition: ScoreDefinition): Draft` | `EMPTY_DRAFT` when `score` is undefined or `score.value.data_type !== definition.data_type`. Otherwise `reason` is `score.reason ?? ""`, and `value` is: numeric → `String(value)`; boolean → `"true"` or `"false"`; categorical → the label; text → the text. |
| `isSet(draft: Draft): boolean` | `draft.value.trim() !== ""`. |
| `isDirty(draft: Draft, saved: Draft): boolean` | `draft.value !== saved.value \|\| draft.reason.trim() !== saved.reason.trim()`. |
| `draftError(draft: Draft, definition: ScoreDefinition): string \| undefined` | `undefined` when not `isSet`. Numeric: `Number(value)` not finite → "Enter a number."; with both bounds and outside them → "Enter a number from {min} to {max}."; only `min_value` and below it → "Enter a number of at least {min}."; only `max_value` and above it → "Enter a number of at most {max}.". Boolean: a value other than `"true"`/`"false"` → "Choose Yes or No.". Categorical: a label that is not a category → "Choose one of the categories.". Text: never an error. Print bounds with `String(n)`. |
| `scoreItem(definition, draft, target, context): Schema<"ScoreCreate">` | Called only on a set draft with no `draftError`. `{target: scoreTarget(target), name: definition.name, value}`, plus `reason` (trimmed) when not empty. For trace and span targets, `session_id` when `context.sessionId`, and `user_id` when `context.userId`. For a session target, `user_id` only (the server ignores `session_id` there). `value`: numeric `{data_type: "numeric", value: Number(draft.value)}`; boolean `{data_type: "boolean", value: draft.value === "true"}`; categorical `{data_type: "categorical", value: {label, ...(category.value !== undefined ? {value: category.value} : {})}}`, where `category` is the definition's category with that label; text `{data_type: "text", value: draft.value.trim()}`. Never `score_id` or `source`. |
| `rowErrors(error: unknown, sentIds: readonly string[]): Record<string, string>` | From a `400` problem's `errors`: each `field` that matches `/^scores\[(\d+)\]\./` maps to `sentIds[index]` (a definition id). The first message per id wins. Anything else gives `{}`. |
| `SCORE_READ_PATHS: readonly string[]` | `"/api/v1/projects/{project}/scores"`, `"/api/v1/projects/{project}/traces"`, `"/api/v1/projects/{project}/traces/{trace_id}"`, `"/api/v1/projects/{project}/sessions"`, `"/api/v1/projects/{project}/sessions/{session_id}"`. |
| `invalidateScoreReads(queryClient: QueryClient): Promise<void>` | `queryClient.invalidateQueries({ predicate })`, where the predicate keeps queries whose `queryKey[1]` is a string in `SCORE_READ_PATHS`. Same shape as `invalidateProjectReads` in `lib/deletions.ts`. These are the reads that show scores: the score lists, and the trace and session lists and details (their `scores` badges). |

### `test/score-definitions.ts`

```ts
import { HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";

// ANNOTATION_DEFINITIONS: the array given just below, exported from this file.

/** Serves `definitions` as the project's score definitions, for any query. */
export function serveAnnotationDefinitions(
  definitions: Schema<"ScoreDefinition">[] = ANNOTATION_DEFINITIONS,
): void {
  server.use(
    http.get("/api/v1/projects/:project/score-definitions", () =>
      HttpResponse.json({ data: definitions, next_cursor: null }),
    ),
  );
}
```

`ANNOTATION_DEFINITIONS`, with the doc comment `/** The four definitions of the annotation tests (05-m5-annotation.md § Fixed identifiers). */`, is exactly (in this order):

```ts
export const ANNOTATION_DEFINITIONS: Schema<"ScoreDefinition">[] = [
  {
    id: "01900000-0000-7000-8000-000000000601",
    name: "helpfulness",
    data_type: "numeric",
    categories: [],
    min_value: 0,
    max_value: 1,
    description: "How helpful the answer is, from 0 to 1.",
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
  {
    id: "01900000-0000-7000-8000-000000000604",
    name: "note",
    data_type: "text",
    categories: [],
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
  {
    id: "01900000-0000-7000-8000-000000000602",
    name: "resolved",
    data_type: "boolean",
    categories: [],
    description: "Did the answer resolve the question?",
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
  {
    id: "01900000-0000-7000-8000-000000000603",
    name: "tone",
    data_type: "categorical",
    categories: [
      { label: "friendly", value: 1 },
      { label: "neutral", value: 0.5 },
      { label: "rude", value: 0 },
    ],
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
];
```

### `mocks/writes.ts`

These are the dev server's stateless answers (`just web-dev`). Add to `WRITE_OPERATIONS`:
- `{ operationId: "create_scores", method: "post", path: "/api/v1/projects/{project}/scores" }`;
- `{ operationId: "delete_score", method: "delete", path: "/api/v1/projects/{project}/scores/{score_id}" }`.

Add to `RESPONSES`:
- `create_scores`: `HttpResponse.json({ score_ids: scores.map(() => "01900000-0000-7000-8000-000000000310") })`, where `scores` is `body.scores` when it is an array, else `[]`;
- `delete_score: () => noContent()`.

In `writes.test.ts`, the "lists every account and admin write of the contract" test's `covered` condition gains `path.startsWith("/api/v1/projects/{project}/scores")`. That prefix does not match `…/score-definitions` (`scores` vs `score-`) and does not match `POST /api/v1/scores`, which has no project segment. Add `it("answers the score writes")`:
- `POST ${PROJECT}/scores` with two items → 200 and `score_ids` of length 2;
- `DELETE ${PROJECT}/scores/01900000-0000-7000-8000-000000000310` → 204.

### Tests (`lib/annotation.test.ts`)

Build scores as plain `Schema<"Score">` objects. A small `score(overrides)` helper with the fields of `fixtures/api/dataset/scores.json`'s first entry is enough.
- `targetKey and scoreTarget cover the three targets`.
- `ownScores keeps the caller's live human scores on the target`. Input:
  - Ada `human` on trace 1 `helpfulness` → kept;
  - Bob `human` on trace 1 → dropped;
  - Ada `api` → dropped;
  - Ada `human` `deleted: true` → dropped;
  - Ada `human` on span 4 of trace 1 → dropped for the trace target, kept for the span target;
  - two Ada `human` `note` scores → the newer one;
  - Ada `human` on session `sess-001`, with a session target → kept.
- `draftOf renders each type`: numeric 0.7 → `"0.7"`; boolean false → `"false"`; categorical → the label; text → the text; reason copied; a type mismatch → `EMPTY_DRAFT`; `undefined` → `EMPTY_DRAFT`.
- `isDirty compares the value and the trimmed reason`.
- `draftError checks numbers, ranges, booleans and categories`: every message of the table, and `undefined` for an unset draft and for text.
- `scoreItem builds the items`:
  - trace numeric with context → `{target: {type: "trace", trace_id}, session_id: "sess-001", user_id: "user-42", name: "helpfulness", value: {data_type: "numeric", value: 0.8}}`;
  - span boolean;
  - categorical with mapping → `{label: "neutral", value: 0.5}`; a category without a value → `{label}` only;
  - session target → no `session_id` key, `user_id` present;
  - a reason "  ok  " → `"ok"`; an empty reason → no `reason` key.
- `rowErrors maps field errors to definitions`: errors `scores[1].value` and `scores[1].name` with sent ids `[a, b]` → `{[b]: <the first message>}`; a non-problem error → `{}`.
- `invalidateScoreReads invalidates only score reads`:
  - In a `new QueryClient()`, `setQueryData` on the keys `["get", "/api/v1/projects/{project}/scores", {}]`, `["get", "/api/v1/projects/{project}/traces/{trace_id}", {}]` and `["get", "/api/v1/projects/{project}/evaluators", {}]`.
  - Call it.
  - `getQueryState(key)?.isInvalidated` is true for the first two and false for the third.

### Done when

`just check-web` passes; then `just check`.

---

## I5.3 — Web: the annotation form and the trace view's Annotate tab

**Goal:** `AnnotationForm` (D-I-37), and an **Annotate** tab in the span detail with a Trace/Span switch.

**Edit only:**
- `web/src/components/annotation/annotation-form.tsx` (new);
- `web/src/components/trace/annotate-tab.tsx` and `.test.tsx` (new);
- `web/src/components/trace/span-detail.tsx` and `span-detail.test.tsx` (new cases only).

**Commit:** `Annotate traces and spans from score definitions`

### `components/annotation/annotation-form.tsx`

```tsx
export function AnnotationForm({
  target,
  context,
}: {
  target: AnnotationTarget;
  context: AnnotationContext;
})
```

The caller renders it with `key={targetKey(target)}`.

**Data:**
- `const { org, project, ref, data } = useProject();`
- `useQuery(meQuery)`, `useQuery(liveScoreDefinitionsQuery(org, project))`, `useQuery(scoreListQuery(org, project, target))`.
- While any is pending: two `Skeleton` lines. On an error: `problemOf(error)?.title ?? "Could not load the annotation form"` in a `<p className="text-sm">`.

**Split for the rules of hooks.** `AnnotationForm` only runs the three queries and returns the loading, error and empty states below. It calls no `useState`, `useQueryClient` or mutation hook. Once everything has loaded, it renders an inner, non-exported `AnnotationRows({ definitions, own, target, context, userId })` component (`own` is `ownScores(...)`'s map), which owns every `useState`, `useQueryClient`, both mutations and the form markup below. So no hook sits after an early return. The caller keys `AnnotationForm` by `targetKey(target)`, so `AnnotationRows`' state resets for each target.

**States, in this order:**
1. `!canAnnotate(data.role)` → `<p className="text-xs text-muted-foreground">Only members can annotate.</p>`. Keep the three query hooks at the top of `AnnotationForm`, before this return; it is fine that they load.
2. No live definitions → `<p className="text-xs text-muted-foreground">No score definitions yet.</p>` and a `Link` "Add score definitions" to `/$org/$project/settings/score-definitions` (params `{org, project}`).
3. Otherwise `<AnnotationRows … />`, which holds the form state and markup below.

**Form state (in `AnnotationRows`):**
- `saved: Map<definitionId, Draft>`: `draftOf(own.get(definition.name), definition)` for each definition, where `own = ownScores(scores.data.data, target, me.data.user.id)`.
- `edits: Record<definitionId, Draft>` (`useState({})`). A row's draft is `edits[id] ?? saved.get(id) ?? EMPTY_DRAFT`.
- `changed` = the definitions whose draft `isSet` and `isDirty(draft, savedDraft)`.
- `errors` = `draftError` per row, merged over `serverErrors` (a `useState<Record<string, string>>({})` filled by `rowErrors`). A local error wins; editing a row removes its server error.
- `success` (boolean) and `formError` (string or undefined), both cleared by any edit and by a new submit.

**Markup:**
- `<form aria-label="Annotations" className="flex flex-col gap-3" onSubmit={save}>`.
- One `DefinitionRow` per definition, sorted by `name.localeCompare`. `DefinitionRow` is a component in the same file (not exported) that takes the definition, its draft, its saved own score (or undefined), its error, and the change and clear callbacks. It calls `useId()` once at its top and builds every id from it: `${id}-${value}` for each radio (`${id}-true`, `${id}-false`, or `${id}-${index}` for the category at `index`), and `${id}-error` for the error `<p>` that `aria-describedby` points at. Do not call `useId` in a loop or per option. A row renders a `<fieldset>` with `<legend className="text-xs font-medium">{name}</legend>`, then the description as `<p className="text-xs text-muted-foreground">` when present.
- The control, by `data_type`. Every control gets `aria-invalid` when its row has an error, and `aria-describedby` pointing at the error `<p>`.
  - numeric: `<Input type="number" step="any" inputMode="decimal" aria-label={name} min={min_value} max={max_value} value onChange>`, with the range under it as muted text: "{min} – {max}", "≥ {min}", "≤ {max}", or nothing. The same wording is used by `rangeOf` in `routes/$org/$project/settings/score-definitions.tsx`; copy the three-line logic, do not import a route module.
  - boolean: `<RadioGroup aria-label={name} value={draft.value} onValueChange className="flex gap-3">` with `RadioGroupItem value="true"` labelled "Yes" and `value="false"` labelled "No". Each item has `<Label htmlFor>` with the row's ids (`${id}-true`, `${id}-false`), as `create-token-dialog.tsx` pairs its items and labels.
  - categorical: the same `RadioGroup`, one item per category, in the definition's order, labelled by the category label, wrapped (`flex flex-wrap gap-3`).
  - text: `<Textarea aria-label={name} rows={2}>`.
- Then the reason: `<Input aria-label={`${name} reason`} placeholder="Reason (optional)">`.
- When the row has a saved own score: a `Button` (`variant="ghost"`, `size="xs"`, `type="button"`) labelled "Clear", with `aria-label={`Clear ${name}`}`.
- The row's error: `<p id={`${id}-error`} className="text-xs text-destructive">`.
- After the rows: `<FormError message={formError} />`, `<FormSuccess message={success ? "Saved." : undefined} />`, and a `Button type="submit" size="sm"` "Save", disabled when `changed` is empty, when any changed row has a `draftError`, or while a mutation is pending.

**Save:**
- `const create = $api.useMutation("post", "/api/v1/projects/{project}/scores");`
- `const sent = changed` (the definition objects, in name order); body `{scores: sent.map((d) => scoreItem(d, rowDraft(d.id), target, context))}`, where `rowDraft(id)` is the row's current draft (`edits[id] ?? saved.get(id) ?? EMPTY_DRAFT`); `params: {path: {project: ref}}`.
- On success: `await invalidateScoreReads(queryClient)`, then `setEdits({})`, `setServerErrors({})`, `setSuccess(true)`. Awaiting the refetch first keeps the saved values on screen, instead of flashing the old ones.
- On error: `const rows = rowErrors(error, sent.map((d) => d.id))`. When `rows` is not empty, `setServerErrors(rows)`. Otherwise `setFormError(problemMessage(error, "Could not save the annotations."))`, which shows the server's detail for `402 quota_exceeded` and for `403`.

**Clear:**
- `const remove = $api.useMutation("delete", "/api/v1/projects/{project}/scores/{score_id}");` with `params: {path: {project: ref, score_id: own.get(name).score_id}}`.
- On success: `await invalidateScoreReads(queryClient)`, then remove that row's edit and server error.
- On error: `setFormError(problemMessage(error, "Could not clear the score."))`.

### `components/trace/annotate-tab.tsx`

```tsx
export function AnnotateTab({ traceId, spanId }: { traceId: string; spanId: string })
```

- `const trace = useQuery(traceDetailQuery(org, project, traceId));`. Its `trace.session_id` and `trace.user_id` are the `context`. It is usually cached by `TraceView`, but not always: never render the form without it, or Save could drop `session_id` and `user_id`. While it is pending, render two `Skeleton` lines; on an error, `<p className="text-sm">{problemOf(trace.error)?.title ?? "Could not load this trace"}</p>`; render the switch and the form only once it has loaded.
- `const [kind, setKind] = useState<"trace" | "span">("trace");`
- A `RadioGroup aria-label="Annotate" value={kind} onValueChange={(value) => setKind(value === "span" ? "span" : "trace")} className="flex gap-3"` with the items "Trace" (`trace`) and "Span" (`span`), above the form.
- `target = kind === "trace" ? {type: "trace", traceId} : {type: "span", traceId, spanId}`.
- `<AnnotationForm key={targetKey(target)} target={target} context={{sessionId: trace.data.trace.session_id, userId: trace.data.trace.user_id}} />` (after the pending and error returns, `trace.data` is defined). Keep every hook (`useQuery`, `useState`) above those early returns.

### `components/trace/span-detail.tsx`

- `SpanTab` becomes `"chat" | "io" | "attributes" | "scores" | "annotate"`. `select`, `readStoredTab` and `activeTab` accept `"annotate"` like `"scores"`.
- A `TabsTrigger value="annotate"` "Annotate" goes after "Scores".
- A `TabsContent value="annotate"` (same classes as Scores) renders `<AnnotateTab traceId={traceId} spanId={spanId} />`.

Radix renders only the active tab's content, so the form's queries run only when Annotate is open; tests that never open it make no new request.

### Tests (`components/trace/annotate-tab.test.tsx`)

**Setup:**
- Every test calls `serveAnnotationDefinitions()` (I5.2), except where noted.
- `const URL = /acme/support-bot/traces?trace=${TRACE}&span=${SPAN}` (trace 1, span `b100000000000014`).
- A helper `openAnnotate()`: `renderApp(URL)`, find `region` "Span", `fireEvent.mouseDown` its "Annotate" tab, and return `{ user, panel, form: await within(panel).findByRole("form", { name: "Annotations" }) }`.
- A helper `captureScores(answer)`: `server.use(http.post("/api/v1/projects/:project/scores", …))` that records bodies and answers `answer()`. The default answer is `HttpResponse.json({ score_ids: ["01900000-0000-7000-8000-000000000310"] })`.
- `own` scores: a `server.use(http.get("/api/v1/projects/:project/scores", …))` that answers `{data: [ADA_SCORE, BOB_SCORE], next_cursor: null}`.
  - `ADA_SCORE` is `helpfulness` 0.7, `human`, `author_user_id: ADA_ID`, trace 1, id `…0310`.
  - `BOB_SCORE` is the same with 0.2, `BOB_ID`, id `…0311`.
  - Build both by spreading the first entry of `fixtures/api/dataset/scores.json` (trace 1, `api`) and overriding: `score_id`, `name: "helpfulness"`, `value: {data_type: "numeric", value: 0.7}` (0.2 for Bob), `source: "human"`, `author_user_id`. That entry has no `reason`, `evaluator_id` or `rule_id`, so neither score has them.

**Cases:**
- `shows one control per live definition`: `spinbutton` "helpfulness", `radiogroup` "resolved" with radios "Yes" and "No", `radiogroup` "tone" with three radios, `textbox` "note", the description of helpfulness, and "0 – 1". "Save" is disabled. The `radiogroup` "Annotate" has "Trace" checked.
- `prefills the caller's own score only`: with `own`, the `spinbutton` "helpfulness" has value `0.7` (not 0.2). The button "Clear helpfulness" exists; "Clear tone" does not.
- `saves the changed rows of the trace without a score id`:
  - With `captureScores`, type 0.8 into "helpfulness", type "clear answer" into "helpfulness reason", and click the radio "neutral" in "tone". Click "Save".
  - The one body is exactly `{scores: [{target: {type: "trace", trace_id: TRACE}, session_id: "sess-001", user_id: "user-42", name: "helpfulness", value: {data_type: "numeric", value: 0.8}, reason: "clear answer"}, {target: {type: "trace", trace_id: TRACE}, session_id: "sess-001", user_id: "user-42", name: "tone", value: {data_type: "categorical", value: {label: "neutral", value: 0.5}}}]}`.
  - "Saved." appears.
  - A `GET …/scores` is received after the `POST`: record the GET count in the `own` handler and assert it grows.
- `annotates the selected span`: click radio "Span", click "Yes" in "resolved", click Save. The body is `{scores: [{target: {type: "span", trace_id: TRACE, span_id: SPAN}, session_id: "sess-001", user_id: "user-42", name: "resolved", value: {data_type: "boolean", value: true}}]}`.
- `refuses an out-of-range number`: type 2 into "helpfulness" → "Enter a number from 0 to 1." is shown, the field has `aria-invalid="true"`, Save is disabled, and no POST is captured.
- `shows a server error on its row`: `captureScores(() => problemResponse(400, "bad_request", { detail: "invalid scores", errors: [{ field: "scores[0].value", message: "value is outside the score definition's range" }] }))`. Type 0.5 and Save → the message is shown in the helpfulness fieldset (`within(screen.getByRole("group", { name: "helpfulness" }))`).
- `shows the units cap`: `problemResponse(402, "quota_exceeded", { detail: "Monthly units cap reached; resets on 2026-02-01." })` → that text in the form's `alert`.
- `clears the caller's own score`: with `own`, and a `server.use(http.delete("/api/v1/projects/:project/scores/:score_id", …))` that records `params.score_id` and answers 204. Click "Clear helpfulness" → the recorded id is `…0310`.
- `a viewer cannot annotate`: `server.use(http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role: "viewer" })))`, as `run-evaluator-dialog.test.tsx` does. Open the tab → "Only members can annotate." and no "Save" button.
- `says when there are no definitions`: `serveAnnotationDefinitions([])` → "No score definitions yet." and a link "Add score definitions" with `href` `/acme/support-bot/settings/score-definitions`.

`span-detail.test.tsx`, new case: `remembers the Annotate tab`. Open the "Annotate" tab; it is `selected`, and `localStorage.getItem(`${STORAGE_PREFIX}span-tab`)` is `"annotate"` (import `STORAGE_PREFIX` from `@/product`). Serve the definitions with `serveAnnotationDefinitions()` so the tab renders its form.

The existing `scores-tab.test.tsx` and `span-detail.test.tsx` cases pass unchanged. There is no test of `AnnotateTab`'s pending state: the tab lives inside `TraceView`, which cannot render before the same trace read, so the state is reached only outside it; the Sonnet review checks it by reading the code.

### Done when

`just check-web` passes; then `just check`. Optional: `just web-dev` shows the tab on the mock data (one `relevance` definition); say in the report whether you looked.

---

## I5.4 — Web: the session sheet; specs and roadmap

**Goal:** the session page's **Annotate** button and sheet; spec 11 and the roadmap describe M5.

**Edit only:**
- `web/src/components/session/session-view.tsx`;
- `web/src/routes/$org/$project/sessions/$sessionId.test.tsx` (new cases only);
- `docs/spec/06-evaluation.md` (the table row only);
- `docs/spec/11-ui.md`;
- `docs/roadmap.md` (only the M5 status in the I row);
- Nothing in `docs/mvp-decisions.md` or `docs/plans/i-online-eval/00-design.md`: only check that D-I-36 and D-I-37 are there and that D-I-26's ownership bullet points at D-I-36. If not, say so in the report; do not add them.

**Commit:** `Annotate sessions and document the annotation form`

### `session-view.tsx`

`SessionHeader` gets, at the end of its first row (`ml-auto`), an "Annotate" button:
- `Button variant="outline" size="xs" type="button"`;
- wrapped in `DisabledReason reason={canAnnotate(data.role) ? undefined : "Only members can annotate"}` and `disabled` for viewers, as `trace-header.tsx` does for "Delete trace". `data` comes from `useProject()`. The root route already provides a `TooltipProvider`.

It opens a `Sheet` (`@/components/ui/sheet`) controlled by a `useState` boolean:

```tsx
<SheetContent side="right" className="overflow-y-auto">
  <SheetHeader>
    <SheetTitle>Annotate session</SheetTitle>
    <SheetDescription>{session.session_id}</SheetDescription>
  </SheetHeader>
  <div className="px-4 pb-4">
    <AnnotationForm
      key={targetKey({ type: "session", sessionId: session.session_id })}
      target={{ type: "session", sessionId: session.session_id }}
      context={{ userId: session.user_id }}
    />
  </div>
</SheetContent>
```

Check the padding classes against `SheetHeader` in `components/ui/sheet.tsx` and match it.

### Spec

`docs/spec/06-evaluation.md` § Scores, in the table, the row "UI annotation panel (human, with score definitions as controls)" becomes "UI annotation form on traces, spans and sessions (one control per live score definition; only the caller's own scores, D-I-37)".

`docs/spec/11-ui.md`:
- § Trace view, **Right (selected span)**: replace the *Scores* bullet with two bullets:
  - "*Scores*: the span's and the trace's scores (name, value, source, reason, author), read-only; an evaluator score whose metadata has `judge.trace_id` links to that trace ("View judge trace")."
  - "*Annotate* (D-I-26, D-I-37): a form built from the project's live score definitions, one control per definition (a number within its range, Yes/No, one of the categories, or text) with an optional reason. A Trace/Span switch picks the target (the trace by default). It shows and edits only the caller's own `human` scores: Save writes the changed rows in one `POST /scores` without `score_id` (so each annotator keeps one score per name and target), and Clear deletes the caller's own score. Trace and span annotations carry the trace's session and user ids. Viewers see "Only members can annotate."; with no definitions, the tab links members to Score definitions. Another member's `human` score can be changed only by a project admin, through the API (D-I-36)."
- § Sessions: replace "The annotation form waits for its API contract (own plan; D123)." with "The header's **Annotate** button opens the same form on the session in a side sheet (members and above; D-I-37)."
- § Settings, **Project**: replace "Score definitions are listed under Project settings (track I, D-I-26)" with "Score definitions are listed under Project settings and drive the annotation form (track I, D-I-26, D-I-37)". Keep the rest of the sentence.

### Roadmap

`docs/roadmap.md`, the I row's status: change only the M5 part, so it says M5 (annotation form on traces, spans and sessions; score ownership, D-I-36–D-I-37) is implemented on branch `<your branch>`. Leave the rest of the row as it is (the plans PR has already brought it up to date).

### Tests (`sessions/$sessionId.test.tsx`)

- `annotates the session`:
  - Call `serveAnnotationDefinitions()`. Capture `POST …/scores` as in I5.3.
  - Render `/acme/support-bot/sessions/sess-001`, click "Annotate", and find `dialog` "Annotate session".
  - In the `radiogroup` "resolved", "Yes" is checked: the fixture score `…0305` is Ada's `human` session score, served by the manifest's `list_scores` answer.
  - Click "No", then Save. The body is exactly `{scores: [{target: {type: "session", session_id: "sess-001"}, user_id: "user-42", name: "resolved", value: {data_type: "boolean", value: false}}]}`, with no `session_id` key at the top level.
  - "Saved." appears.
- `a viewer cannot annotate the session`: project role `viewer` → the "Annotate" button is disabled.

The existing session tests pass unchanged: the sheet's queries run only once it is open.

### Done when

`just check` passes. `rg -n "annotation form waits" docs/spec/11-ui.md` is empty.

---

## After M5

Report: the four commits, deviations, and anything unverified. `just check` is green on the merged branch, and the I row of the roadmap says M5 is implemented.

## Open questions (with the defaults this plan uses)

1. **An admin's overwrite takes authorship.** Default: yes. The stored row is a new version written by the admin, so `author_user_id` is the admin's (D-I-36). Keeping the original author would need a writer that writes on someone else's behalf, which G's model does not have.
2. **No admin UI for other people's scores.** Default: the Scores tab stays read-only for everyone, and admins use the API. A "Delete" action for admins on another member's human score is a small follow-up if wanted.
3. **Default target in the trace view.** Default: the trace. Most competitors annotate the trace first; the switch is one click.
4. **More than 1 000 scores on one target.** Default: the form reads at most 1 000 scores of the target, as the Scores tab does. A caller's own score beyond them is not prefilled; saving still replaces it, because the derived id is the same.
5. **Annotating a name with no definition.** Default: not in the form; the API still accepts it.
