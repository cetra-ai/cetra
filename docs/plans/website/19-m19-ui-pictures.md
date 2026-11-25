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
| P1 Support prompt v3 | `web/src/mocks/stories-prompts.ts`, `stories-prompts.test.ts`, `web/src/mocks/playground.ts`, `playground.test.ts`, `docs/website/00-design.md` (§ 13, the Prompts · Support cell) | — |
| P2 Issue rule and batch | `web/src/mocks/generated/stories.ts`, `stories.test.ts`, `web/src/mocks/generated/story-extraction.ts`, `story-extraction.test.ts` | — |
| P3 Screenshots and texts | `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, `stories.test.mjs`, PNGs, docs pages' alts (only where a docs shot changed), `docs/plans/website/README.md` | P1, P2 |

P1 and P2 touch disjoint files and can run in parallel (no `pnpm typecheck` while both run; vitest and Biome only; commit one after the other). **Checks:** as in M18's plan.

---

## P1 Support prompt v3

1. `stories-prompts.ts`: under `acme/support-bot` only, `support-reply` gets a version 3 on top of the fixture's two: the versions list (`GET prompts/support-reply/versions`) returns the fixture's v1 and v2 (read them from `fixtures/api/responses/prompts/support-reply/versions.json` and `versions/`) plus v3; `GET …/versions/3` returns v3; `GET prompts/support-reply` (and the list row) reports latest version 3 with the `production` label on it. Other projects keep the fixture as today. v3 is v2's template with one more line at the end of the system message: "If the customer asks for something cetra doesn't do, say so plainly, then offer the closest thing it does." Commit message "Say plainly what cetra doesn't do", `at` one day after v2's, the same model and params as v2.
2. `playground.ts`: for `support-reply`, `source.prompt.version` 2 answers `SCHEDULE_ANSWER` (import it from `story-support.ts`: "You can export any time from the Traces page with Export → CSV."), version 3 answers "cetra can't schedule exports yet. You can export by hand any time from the Traces page with Export → CSV, or call the export API from your own weekly job." with tokens 1,060/52, cost $0.0042, latency 1,300 ms, ttft 300 ms; the v2 answer keeps the fixture's numbers if it has them, else tokens 1,040/24, $0.0035, 1,100 ms, ttft 280 ms. `trace_id`: the third turn of `sess-0142` (index 9 of the support dataset: its trace id).
3. `docs/website/00-design.md` § 13, the Prompts · Support cell: "v12 and v13" becomes "v2 and v3".
4. Tests: the support project's versions list has 1, 2, 3; another project keeps 1, 2; the playground answers per version; `stories-prompts.test.ts`' pinned list (line ~51) follows (support-reply appears once in each project).

**Commit:** `Give the support prompt the version that fixes the schedule answer`

## P2 Issue rule and batch

1. `story-extraction.ts`: the three batch failures move from ranks 41, 97 and 158 to ranks 191, 195 and 198 (the newest in the batch, so the default newest-first list shows them in its first ten rows). Update the comment; the facts keep their values (`batchFailedTwice` 3; re-pin `batchCost` if it moves and report old and new).
2. `stories.ts`, rule 570204: name "Scanned PDFs over 10 an hour", condition `{ metric: "issue_occurrences", issue_id: <issue 570104's id>, window: "1h", comparator: "gt", threshold: 10 }` (no `min_count`, no filter), firing, `lastValue` = the occurrences of issue 570104 in `[now − 1 h, now)`, `lastCount` the same, `changedAgoMs` = `now` minus the start of the first whole hour over 10 in the last 24 h (the last hour at the clock), channel Team Slack.
3. `stories.ts`: serve `GET alert-rules/issue-preview` (query `issue_id` or `source_id`, `window`; the shape the survey gives: `{ from, to, window, points: [ { time, count } ] }`, buckets aligned to the window as the server does) for the story issues under their project: `count` = the issue's matching visible traces that start in the bucket. Register it before the `alert-rules/:id` route so the path isn't read as an id. Unknown ids fall through.
4. Tests: rule 570204's value is above 10 at the clock and its count equals the last hour's occurrences; the preview's last bucket is that count and every earlier bucket of the last 24 h is at most 10; the batch's three failures are its three newest traces.

**Commit:** `Alert on scanned invoices and show the batch's failures first`

## P3 Screenshots and texts

1. Entries (keep `theme: "dark"`; only `click`/`type` actions):

| Shot | Path | Actions | waitFor / waitAfter | Viewport, clip |
|---|---|---|---|---|
| marketing-playground | `/acme/support-bot/playground?prompt=support-reply&version=2&compare=3` | type the variables (product "cetra", language "English", question "Can I schedule it every week?"), click "Run all" | waitFor "Run all", waitAfter "can't schedule exports yet" | 1400×800, `main` |
| marketing-alerts-extraction | `/acme/invoice-pipeline/alerts?rule=…570204` | click `button[aria-label="Bottom"]` | waitFor "Scanned PDFs over 10 an hour", waitAfter "History" | 900×900, `main` |
| marketing-trace-page-extraction | `/acme/invoice-pipeline/traces?filter=tags%20%40%3E%20%5B'batch-1119'%5D&cols=start,name,duration,cost` | — | waitFor "200 traces" (the totals line; use its exact text from the UI) | 900×700, `main` |

   Read the variable names of support-reply in the fixture; if a waitFor text isn't what the UI prints, use the text it does print and report it.
2. Retake the shots the merge changed (survey § 4): `marketing-playground-coding`, `-voice`, `-extraction`, `marketing-alerts`, `-coding`, `-voice`, `marketing-assistant*`, and the docs shots `playground`, `prompts`, `prompt`, `trace-list`, `trace-view`, `alerts`.
3. Look at every PNG that changed; write the three pictures' alts and captions from them and update `EXPECTED`; check the retaken ones still match their texts (fix only what doesn't); fix a docs alt only where its picture no longer matches.
4. `docs/plans/website/README.md`: M19 done.
5. Checks: `node --test src/marketing/data/stories.test.mjs`, `just check-docs`.

**Commit:** `Take the last three story pictures`

## After P3 (the coordinator)

A Sonnet code review of P1 and P2, fixes, `just check-web`, push.
