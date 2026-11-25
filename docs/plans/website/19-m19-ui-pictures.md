# M19 Pictures that wait for the UI

The last three homepage pictures, now that their UI features are on main: Prompts · Support (prompt versions side by side, #259), Alert · Extraction (the occurrences chart of an issue rule, #261) and Trace · Extraction (the trace list's totals line, #260). The mocks gain what each picture needs, and the M18 shots that the merged features changed are retaken.

**Prerequisites:** M18 done and main merged (branch `claude/focused-screenshots`, head 7a217d3 or later).

**Read first:**
- `docs/website/00-design.md` § 13 (the picture table, the rules, M19).
- `docs/plans/website/18-m18-story-pictures.md` (how a picture is built: mocks, `website/screenshots.json`, `website/src/marketing/data/stories.mjs`).
- The survey `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/m19-code-facts.md` (routes, components, endpoints, shapes, current mocks and screenshot entries, file:line).
- `web/src/mocks/stories-prompts.ts`, `web/src/mocks/playground.ts`, `web/src/mocks/fallbacks.ts`, `fixtures/api/responses/prompts/support-reply/`, `web/src/mocks/generated/stories.ts`, `web/src/mocks/generated/story-extraction.ts`, `web/src/mocks/generated/story-support.ts` (`SCHEDULE_ANSWER`).

## What it is, and what it is not

When it's done, the three pictures show what design § 13 says, with texts written from them, and every M18 or docs shot that the merge changed is retaken. Out of scope: the UI layout problems the M18 pictures showed (overlapping badges on the session page and the queue review, the trace panel's header cut at the right): they go to the App UI session.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-26, design § 13 | The three pictures; every number comes from the story data |
| P-1 (planner) | The support prompt compares **v2 against v3**, not "v12 against v13": `support-reply` is a fixture prompt with two versions, which the docs shots show. The support project gets a v3 on top of the fixture's v1 and v2 (v2 misses the weekly schedule, v3 answers right); the version numbers in design § 13 were illustrative. Design § 13 is updated in P1. |
| P-2 (planner) | The batch picture uses the totals line of the filtered list (the line #260 shows with nothing selected: traces, errors, cost), so no selection action is needed. The three failed invoices move to the batch's newest ranks so the default sort shows them on the first screen. |
| P-3 (planner) | The extraction alert rule 570204 becomes the issue rule (the schema-check rule it replaces appears in no picture). |

## Phases

| Phase | Edit | Needs |
|---|---|---|
| P1 Support prompt v3 | `web/src/mocks/stories-prompts.ts`, `stories-prompts.test.ts`, `web/src/mocks/playground.ts`, `playground.test.ts`, `web/src/mocks/generated/story-support.ts` (export two constants), `docs/website/00-design.md` (§ 13, the Prompts · Support and Trace · Extraction cells) | — |
| P2 Issue rule and batch | `web/src/mocks/generated/stories.ts`, `stories.test.ts`, `web/src/mocks/generated/story-extraction.ts`, `story-extraction.test.ts` | — |
| P3 Screenshots and texts | `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, `stories.test.mjs`, PNGs, docs pages' alts (only where a docs shot changed), `docs/plans/website/README.md` | P1, P2 |

P1 and P2 touch disjoint files and can run in parallel (no `pnpm typecheck` while both run; vitest and Biome only; commit one after the other). **Checks:** as in M18's plan.

---

## P1 Support prompt v3

1. `story-support.ts`: export `SCHEDULE_QUESTION` and `SCHEDULE_ANSWER` (today plain `const`).
2. `stories-prompts.ts`: `support-reply` becomes a **story prompt of the support story** (a `STORY_PROMPTS` entry with `story: "support"`), so every prompt surface (the prompt, its versions, one version, `resolve`, `label-events`, `metrics`) comes from `storyPromptHandlers` under `acme/support-bot`; in every other project the fixture answers as today. The list handler drops the fixture row whose name a served story prompt has, so `support-reply` appears once. Its versions:
   - v1 and v2: the fixture's templates, variables, model and params, read from `fixtures/api/responses/prompts/support-reply/versions/` (their dates as in the fixture). The fixture's v2 template has a placeholder message (`history`): extend the story template entry type with `{ kind: "placeholder"; name: string }` beside the role messages, and have the version route render it as the fixture's version does (compare the JSON).
   - v3: v2's template with one more line at the end of the system message: "If the customer asks for something cetra doesn't do, say so plainly, then offer the closest thing it does." Commit message "Say plainly what cetra doesn't do", `at` `2026-01-01T10:30:00Z` (before the docs shots' clock), v2's model and params.
   - Labels: v3 `production` (so it is latest and production), v2 `staging`, v1 none. Label events: the fixture's, then `production` moved to v3 at v3's `at`. Metrics: per version, as the other story prompts' metrics are built.
3. `playground.ts`, `support-reply`: version 3 answers "cetra can't schedule exports yet. You can export by hand any time from the Traces page with Export → CSV, or call the export API from your own weekly job." (tokens 1,060/52, cost $0.0042, latency 1,300 ms, ttft 300 ms); version 2 answers `SCHEDULE_ANSWER` (tokens 1,040/24, $0.0035, 1,100 ms, ttft 280 ms) **when the `question` variable is `SCHEDULE_QUESTION`**, and today's answer otherwise (so the docs `playground` shot, which types no question, keeps its answer). Both new answers' `trace_id` is `a0000000000000000000000000000009` (the third turn of `sess-0142`, support index 9): pin it in the test from the support story dataset.
4. `docs/website/00-design.md` § 13: the Prompts · Support cell says "v2 and v3"; the Trace · Extraction cell says the list shows each invoice's status and the batch's totals line (200 traces, 3 errors, its cost), not a tag.
5. Tests: under `acme/support-bot` the prompt list has `support-reply` once with latest 3; its versions are 1, 2, 3 with labels v3 production and v2 staging; `metrics` and `label-events` answer; another project keeps the fixture (versions 1, 2); the playground answers v3, v2 with the schedule question, and v2 without it (today's answer). Existing tests that change: `stories-prompts.test.ts`' pinned lists (line ~51) and `playground.test.ts`' support-reply case (it expects today's text for v2: keep that case without the schedule question).

**Done when:** from `web/`, `node_modules/.bin/vitest run src/mocks/stories-prompts.test.ts src/mocks/playground.test.ts src/mocks/generated/story-support.test.ts --maxWorkers=4` and Biome on the files.

**Commit:** `Give the support prompt the version that fixes the schedule answer`

## P2 Issue rule and batch

1. `story-extraction.ts`: the three batch failures move from ranks 41, 97 and 158 to ranks **197, 198 and 199** (the batch's newest, so the default newest-first list shows them in its first three rows). Update the comment; `batchFailedTwice` stays 3 and `batchCost` 0.4776 (re-run, report if it moves).
2. `stories.ts`, rule 570204: name "Scanned PDFs over 10 an hour", condition `{ metric: "issue_occurrences", issue_id: "01900000-0000-7000-8000-000000570104", window: "1h", comparator: "gt", threshold: 10 }` (no `min_count`, no filter), firing; `lastValue` and `lastCount` = the number of issue 570104's occurrences (its matching visible traces) that start in `[now − 1 h, now)`; `changedAgoMs` = `now` minus the start of the last hour bucket (at the clock: 0, so use the bucket's start: `now − floor(now / 1 h) × 1 h`, and at the screenshot clock show "Firing for <1h"); channel Team Slack. The rule's events: one firing event at that bucket's start with that value.
3. `stories.ts`: serve `GET alert-rules/issue-preview` under the story projects, with the shape `IssueAlertPreview` in `crates/server/openapi.json` (read its field descriptions: `from` is the start of the window-wide bucket holding `to` minus the preview span, 24 h for a 1 h window; `to` is `dataset.now`; `window` echoes the query; `points` one per bucket from `from` to the last that starts before `to`, oldest first, each `{ time: <bucket start, RFC 3339>, count }`). For `issue_id` of a story issue served in the project, `count` = its matching visible traces starting in the bucket; for `source_id`, the sum over that source's story issues; any other id falls through. Register it before `alert-rules/:id`.
4. Tests: at the clock the preview has 24 points, the last 14 and every earlier one at most 6; rule 570204 fires with value 14; the batch's three failures are its three newest traces. Existing tests that change in `stories.test.ts`: the rules' states array (570204 is now firing), the firing/ok counts (now 4 firing, 0 ok), and the "ok rule has fixed values" test (delete it: no ok rule remains).

**Done when:** from `web/`, `node_modules/.bin/vitest run src/mocks/generated/stories.test.ts src/mocks/generated/story-extraction.test.ts --maxWorkers=4` and Biome on the four files.

**Commit:** `Alert on scanned invoices and show the batch's failures first`

## P3 Screenshots and texts

1. Entries (keep `theme: "dark"`; only `click`/`type` actions):

| Shot | Path | Actions | waitFor / waitAfter | Viewport, clip |
|---|---|---|---|---|
| marketing-playground | `/acme/support-bot/playground?prompt=support-reply&version=2&compare=3` (v2 left, v3 right) | `{ "type": ["input[aria-label=\"product\"]", "cetra"] }`, the same for `language` "English" and `question` "Can I schedule it every week?" (use the action syntax of today's marketing-playground entry), then click `button:has-text('Run all')` | waitFor "Run all", waitAfter "can't schedule exports yet" | 1400×800, `main` |
| marketing-alerts-extraction | `/acme/invoice-pipeline/alerts?rule=…570204` | click `button[aria-label="Bottom"]` | waitFor "Scanned PDFs over 10 an hour", waitAfter "History" | 900×900, `main` |
| marketing-trace-page-extraction | `/acme/invoice-pipeline/traces?filter=tags%20%40%3E%20%5B'batch-1119'%5D&cols=status,start,name,duration,cost` | click `button[aria-pressed]:has-text("Filters")` (hides the facet panel, as today's entry) | waitFor "200 traces · 3 errors" (the totals line reads `200 traces · 3 errors · $0.48`) | 900×700, `main` |

   Read the variable names of support-reply in the fixture; if a waitFor text isn't what the UI prints, use the text it does print and report it.
2. Retake the shots the merge changed (survey § 4): `marketing-playground-coding`, `-voice`, `-extraction`, `marketing-alerts`, `-coding`, `-voice`, `marketing-assistant*`, and the docs shots `playground`, `prompts`, `prompt`, `trace-list`, `trace-view`, `alerts`.
3. Look at every PNG that changed; write the three pictures' alts and captions from them and update `EXPECTED`; check the retaken ones still match their texts (fix only what doesn't); fix a docs alt only where its picture no longer matches.
4. `docs/plans/website/README.md`: M19 done.
5. Checks: `node --test src/marketing/data/stories.test.mjs`, `just check-docs`.

**Commit:** `Take the last three story pictures`

## After P3 (the coordinator)

A Sonnet code review of P1 and P2, fixes, `just check-web`, push.
