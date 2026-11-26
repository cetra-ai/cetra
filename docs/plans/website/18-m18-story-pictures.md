# M18 Pictures that need no UI change

The 21 homepage pictures today's UI can show (design § 13), built on M17's story data: the story objects each picture needs (issues, alert rules, a channel, queues, evaluators, an online rule, the invoice dataset and experiments, prompt versions, playground answers, assistant chats, voice audio), the 21 screenshots, and the switcher's names and texts. Every number a picture or its text shows is one of M17's pinned facts.

**Prerequisites:** M17 done (branch `claude/focused-screenshots`, head 8e79ded or later).

**Read first:**
- `docs/website/00-design.md` § 13: the picture table, the rules, the answers of 2026-11-25, M17-M19.
- `docs/plans/website/17-m17-story-data.md` and the four story modules `web/src/mocks/generated/story-{support,coding,voice,extraction}.ts` with their tests (the facts and their pinned values).
- The survey `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/m17-code-facts.md`: one section per picture with the route, the components, the endpoints and the current mocks (file:line). Each phase below names its section.
- `web/src/mocks/generated/stories.ts` (`StoryIssue`, `StoryRule`, `StoryQueue`, `storyHandlers`), `web/src/mocks/assistant.ts` (`TurnSpec`, `STORIES`, `searchTurn`, `dashboardTurn`, `storyEvents`, `storyMessages`), `web/src/mocks/stories-prompts.ts`, `web/src/mocks/playground.ts`, `web/src/mocks/generated/experiments.ts`, `web/src/mocks/projects.ts`, `web/src/main.tsx`, `web/src/mocks/browser.ts`.
- `website/screenshots.json`, `website/src/marketing/data/stories.mjs` and its test, `website/STYLE.md`.

## What it is, and what it is not

When it's done, `just docs-screenshots` writes the 21 pictures below, each shows what its row of design § 13 says, and the switcher's alts and captions describe them.

Out of scope: the three M19 pictures (Prompts · Support, Alert · Extraction, Trace · Extraction keep their M16 shots until M19); the hero pictures (unchanged); the docs pages' screenshots (unchanged unless a mock change moves them: P9 retakes and checks them).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-26 | A picture per story; numbers come from the story project's data (design § 13) |
| Answers 2026-11-25 | Issues show the kind through the source's name; voice judge on the Configuration tab with a test result; coding Evaluate on the online rule page's hourly line; the assistant's action is the Filter card's "Open in traces" (P-5); the coding cost step is inside the chart's 24 hours (design § 13) |
| P-1 (planner) | Story objects stay in `stories.ts`, `stories-prompts.ts`, `assistant.ts` and `playground.ts`, and new kinds of story objects (evaluators, online rules, channels, the invoice dataset and experiments) go in one new module, `generated/story-evaluate.ts`, served by `storyHandlers` per project like the others (M16 P-4: only under the story's project). |
| P-2 (planner) | Texts quote M17's pinned facts as rounded numbers (62%, 41, 18%, 70%, ×2.7, 1.5 s, 3.5 s, 71%, 6%, 38%, 214, 30). A test per story module checks each quoted number against `facts(dataset)` at the screenshot clock, so a data change can't leave a text wrong. |
| P-4 (planner) | Every new story handler follows `storyHandlers`' convention: given a project ref it answers only that project's story objects, and the fixture objects stay only in `acme/support-bot`; given no project (the tests' unscoped call) it answers every story object. |
| P-5 (planner) | The Explore pictures end on the Filter card's "Open in traces" only; the dashboard turns are dropped (the Dashboard card stays an action of the product, not of these pictures). |
| P-3 (planner) | The pictures whose page differs per story (Trace, Evaluate) get neutral switcher names: `section` "Trace" and "Evaluate", `base` `marketing-trace-page` and `marketing-evaluate`; shot names stay `base` + `-<story>` (support has none). |

## Versions

No new dependency.

## The pictures

| Chapter | Story | Page and state | Phase |
|---|---|---|---|
| Trace | Support | `/acme/support-bot/sessions/sess-0142`, clip `section[aria-label="Conversation"]` | P8 |
| Trace | Coding | `/acme/code-agent/traces?trace=a…04&mode=transcript`, clip `aside[aria-label="Trace"]` | P8 |
| Trace | Voice | `/acme/voice-desk/traces?trace=<voice turn>&span=<draft reply span>`, the span's Chat tab | P2, P8 |
| Explore | all four | the assistant window alone over the project's traces page, one stored chat each | P3 |
| Issues | all four | `/acme/<project>/issues/<id>` | P4 |
| Evaluate | Support | the annotation queue review of the conversation (session target) | P5 |
| Evaluate | Coding | the online rule drawer (`/acme/code-agent/online-rules?rule=<id>`), its "Scores, last 24 hours" line | P5 |
| Evaluate | Voice | the LLM judge's Configuration tab with a test result | P5 |
| Evaluate | Extraction | `experiments/compare?ids=<v3>,<v4>` | P6 |
| Prompts | Coding, Voice, Extraction | the playground on the new version | P7 |
| Alert | Support, Coding, Voice | `/alerts?rule=<id>` | P4 |

## Phases

| Phase | Edit | Needs |
|---|---|---|
| P1 Switcher names | `website/src/marketing/data/stories.mjs`, `stories.test.mjs`, `website/screenshots.json`, the renamed PNGs | — |
| P2 Voice audio turn | `web/src/mocks/generated/story-voice.ts`, `story-voice.test.ts` | — |
| P3 Assistant chats | `web/src/mocks/assistant.ts`, `assistant.test.ts` | — |
| P4 Issues and alerts | `web/src/mocks/generated/stories.ts`, `stories.test.ts` | — |
| P5 Queue, check and judge | new `web/src/mocks/generated/story-evaluate.ts`, new `story-evaluate.test.ts`, `web/src/mocks/generated/handlers.ts` (registration only), `handlers.test.ts`, `web/src/mocks/generated/stories.ts` (the support queue, `STORY_SCORE_DEFINITIONS`), `stories.test.ts` | P2, P4 |
| P6 Invoice experiment | `web/src/mocks/generated/story-evaluate.ts`, `story-evaluate.test.ts`, `web/src/mocks/generated/experiments.ts`, `experiments.test.ts`, `web/src/mocks/generated/handlers.ts` (the `experimentHandlers()` call) | P5 |
| P7 Prompt versions | `web/src/mocks/stories-prompts.ts`, `stories-prompts.test.ts`, `web/src/mocks/playground.ts`, `playground.test.ts` | — |
| P8 Screenshots and texts | `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, `stories.test.mjs`, PNGs | P1-P7 |
| P9 Docs shots | PNGs and alts of docs pages that changed | P8 |

P1, P2, P3, P4 and P7 touch disjoint files and can run in parallel in one worktree, committing one after another (`git commit -- <files>`; on `index.lock`, wait and retry). While they run in parallel they **don't run `pnpm typecheck`** (it checks the whole project and would see the others' half-done changes): vitest and Biome only; the coordinator type-checks once after the five commits. P5 waits for P2 and P4, P6 for P5.

**Checks in every phase:** `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`; web phases from `web/`: `node_modules/.bin/vitest run <the phase's test files> --maxWorkers=4`, `pnpm typecheck`, `../node_modules/.bin/biome check --write <files>` then without `--write`; website phases: `cd website && node --test src/marketing/data/stories.test.mjs` and `just check-docs`. Never the full suite or `just check`. Delete `web/.vitest/` if created.

---

## P1 Switcher names

**Edit:** `website/src/marketing/data/stories.mjs`, `website/src/marketing/data/stories.test.mjs`, `website/screenshots.json`, the PNGs it renames under `website/src/assets/screenshots/`.

1. The `trace` picture: `section` "Trace", `base` "marketing-trace-page", `label` per story "Session · Support bot", "Trace · Code agent", "Trace · Voice desk", "Traces · Invoice pipeline".
2. The `evaluate` picture: `section` "Evaluate", `base` "marketing-evaluate", `label` per story "Annotation queue", "Online rule", "LLM judge", "Experiments".
3. The `explore` picture keeps "Assistant"; `issues`, `prompts`, `alert` keep theirs.
4. Alts and captions stay as they are in P1 (P8 rewrites them); the shot names now point to `marketing-trace-page[-<story>]` and `marketing-evaluate[-<story>]`, which don't exist yet: P1 also renames the existing entries in `website/screenshots.json` and the PNG files (`git mv`) so the check passes: `marketing-trace-list*` → `marketing-trace-page*`, `marketing-queue-review*` → `marketing-evaluate*`. Add `website/screenshots.json` and the PNGs to this phase's commit.
5. `stories.test.mjs`: the shot-name test covers the new names (line ~110 `shotName("trace", "support")` becomes `"marketing-trace-page"`); `EXPECTED` captions stay.

**Commit:** `Name the story pictures whose page differs per story`

## P2 Voice audio turn

**Edit:** `web/src/mocks/generated/story-voice.ts`, `story-voice.test.ts`. Survey § 1.3.

1. Pick the turn **inside `voiceMarks`**: among the turns that mark 2 (availability checks) gives a `check_availability` span, not touched by mark 1 (hang-ups), starting within `now − 2 h`, the lowest index. Its `Mark.spans` becomes `[draftReply, checkAvailability]` (the `draft reply` span **first** in the list, so `check_availability` stays the trace's last span, which `availabilityMs` reads); `draftReply` has `fromMs: 250 + d`, `durationMs: 300` (it starts when the check ends and ends before the turn's `durationMs`, which stays `d + 600`). Its mark also gets `turn: RESCHEDULE_TURN` = `{ question: "Hi, can I move my appointment to Friday morning?", heading: "Reschedule", summary: "Friday at 9:30 is free. Should I book it?", steps: [], followUps: [ { question: "Yes, please.", summary: "Booked: Friday at 9:30.", steps: [] }, { question: "Thanks.", summary: "You're welcome. Goodbye!", steps: [] } ] }`, so its previews match the audio. Export `voiceAudioTurn(dataset): number`, which finds the same turn from the dataset (tag `calendar`, not tagged `hangup` and not the turn before a `hangup` turn of its call, start within `now − 2 h`, lowest index, and its second-to-last span is `draft reply`).
2. The `draft reply` span is `{ name: "draft reply", kind: "llm", fromMs, durationMs, input, output }`. Payloads are `{ format: "messages", value: [ { role, parts: [...] } ] }` (`crates/server/openapi.json` `Payload`, `Message`). `input.value`: `{ role: "user", parts: [ { type: "uri", uri: CALLER_WAV, modality: "audio", mime_type: "audio/wav", transcript: "Hi, can I move my appointment to Friday morning?" } ] }`, `{ role: "assistant", parts: [ { type: "tool_call", id: "call_1", name: "check_availability", arguments: { day: "Friday", time: "morning" } } ] }`, `{ role: "tool", parts: [ { type: "tool_call_response", id: "call_1", response: { slots: ["9:30", "11:00"] } } ] }`. `output.value`: `{ role: "assistant", parts: [ { type: "uri", uri: AGENT_WAV, modality: "audio", mime_type: "audio/wav", transcript: "Friday at 9:30 is free. Should I book it?" } ] }`. Check each field name against the openapi schemas before writing it.
3. `CALLER_WAV` and `AGENT_WAV`: `data:audio/wav;base64,…` URIs built at module load by a function `toneWav(seconds, seed)` that writes a 16-bit mono PCM WAV at 8,000 Hz (RIFF header, then samples of a few summed sines with a slow amplitude envelope, so the waveform has visible shape): 2.4 s for the caller, 2.0 s for the agent. Write the header bytes with a `DataView`; base64 with `btoa` over the byte string.
4. Tests: `story-voice.test.ts`'s existing pins still hold unchanged (the median, p95, hang-up share, interrupted share, hourly p95); `voiceAudioTurn(dataset at NOW)` is pinned to its index (P8 uses it), and its trace id (`a` + the index in 31 hex digits) and the `draft reply` span id (`spanId(index, span_count − 2)`) are exported as `VOICE_AUDIO_TRACE_ID` and `VOICE_AUDIO_SPAN_ID` constants pinned by the test; the turn is in the last 2 hours with a `calendar` tag; its `draft reply` span's detail has the four parts above with their transcripts; each audio URI decodes to a RIFF/WAVE header of the stated length.

**Commit:** `Give a voice turn its call audio`

## P3 Assistant chats

**Edit:** `web/src/mocks/assistant.ts`, `assistant.test.ts`. Survey § 2.

1. `TurnSpec.draft` becomes `{ kind: "dashboard" | "filter"; draft: Fields }`; `storyEvents` and `storyMessages` emit a `filter` draft as they do a dashboard one (the default script, lines 64-96 and 186-200, shows the filter draft's shape). `Story.turns` becomes a one-element tuple `[TurnSpec]`; `storyDetail` and every reader use `turns[0]`.
2. Each story's chat is **one turn**: the question, a `query_sql` call with `arguments: { query: <SQL>, params: {} }` (`tool-step.tsx` reads `arguments.query`), a `result` object `{ columns: [...], rows: [...] }` holding the rows the SQL returns (the columns its `SELECT` names; counted over eligible traces only, as the facts are; values from the facts as each row of the table says; support: one row per release, `v2.14` with `downvotes` 66 and `old_pricing` 41; coding: two rows, retried true/false with their runs and cost), `rows` = that row count, then the answer, then a `filter` draft `{ view: "traces", filter, range, include_eval: false, include_experiments: false }`. `range` is one of `15m, 1h, 6h, 24h, 7d, 30d` (`web/src/lib/assistant/types.ts` `RANGES`). The SQL runs against the documented schema (`website/src/content/docs/docs/reference/sql-schema.md`: tables `traces`, `spans`, `scores`; `has_score(name)` takes one argument, `score(name)` gives the value, `array_has(tags, 'x')`):

| Story | Question | SQL | Answer | Filter, range |
|---|---|---|---|---|
| Support | Why did our thumbs-down rate double since Tuesday? | `SELECT release, count(*) AS feedback, count(*) FILTER (WHERE score('user_feedback') = 0) AS downvotes, count(*) FILTER (WHERE array_has(tags, 'old-pricing')) AS old_pricing FROM traces WHERE has_score('user_feedback') AND start_time >= now() - INTERVAL '46 hours' GROUP BY release` | **62%** of the downvoted answers since Tuesday 14:00 quote the old pricing page: retrieval returns `pricing-2025.md`, back in the index since deploy v2.14. That's **41** traces. | `release = 'v2.14' and tags @> ['old-pricing']`, `7d` |
| Coding | Why did cost per run triple yesterday? | `SELECT array_has(tags, 'api-retry') AS retried, count(*) AS runs, sum(cost_total) AS cost FROM traces WHERE start_time >= now() - INTERVAL '14 hours' GROUP BY 1` | **18%** of runs call `run_tests` more than five times and make **70%** of the cost: they all touch `packages/api`, whose test suite times out at 30 s, so the agent retries. | `tags @> ['api-retry']`, `24h` |
| Voice | Why do callers hang up? | `SELECT count(*) AS checks, approx_percentile_cont(s.duration_ms, 0.5) AS p50_ms, approx_percentile_cont(s.duration_ms, 0.95) AS p95_ms FROM spans s JOIN traces t ON s.trace_id = t.trace_id WHERE s.name = 'check_availability' AND array_has(t.tags, 'calendar') AND t.start_time >= now() - INTERVAL '14 days'`; result one row: `checks` = the number of marked availability spans, `p50_ms` = `availabilityMedianMs`, `p95_ms` = `availabilityP95Ms` | **71%** of hang-ups follow a turn over 2.5 s, almost all of them `check_availability` on the calendar API (median **1.5 s**, p95 **3.5 s**). | `tags @> ['calendar'] and duration_ns > 2500000000`, `24h` |
| Extraction | Which vendors' invoices fail the most, and why? | `SELECT input_preview AS invoice_file, count(*) AS invoices, round(avg(CASE WHEN array_has(tags, 're-extracted') THEN 1.0 ELSE 0.0 END), 2) AS re_extracted FROM traces WHERE start_time >= now() - INTERVAL '30 days' GROUP BY 1 ORDER BY 3 DESC LIMIT 4` (each vendor's invoices share one file name, the one `extractionFacts` maps to the vendor); result rows: the four comma vendors' file names with their counts and shares from `extractionFacts` (Nordic Freight's `invoice-2026-0521.pdf (2 pages)` first) | Nordic Freight: **38%** re-extracted, all on `total`. It writes amounts as "1.234,56" and the schema expects a dot; Baltic Paper, Rhine Tools and Lyon Bureau use the same format. | `tags @> ['re-extracted']`, `7d` (deliberately all vendors) |

   Check each column name and function against the schema page; replace one that doesn't exist by the documented equivalent and report it. The numbers in bold are rounded from the facts (`quotingShare`, `quotingOldPricing`, `retryShare`, `retryCostShare`, `hangupsAfterSlowTurn`, `availabilityMedianMs`, `availabilityP95Ms`, `nordicFreightRetryShare`).
3. Remove `dashboardTurn`, the drafts built from `storyDashboards()`, and the imports only they used (`storyDashboards`, `STORY_DASHBOARD_IDS`, `dashboardDraft`); the story dashboards module stays. In `assistant.test.ts`, delete the tests that only check the dashboard turn or `turns[1]` (lines ~162-180, 216-270, 272-350, 390-450 at the plan's commit: read each and delete it only if it reads the dashboard turn), and rewrite the others to one turn.
4. Tests: per story, the stored chat has the question, a `query_sql` step with `arguments.query`, the answer and a `filter` draft that `parseDraft("filter", draft)` accepts; **each bold number** equals the rounded fact from `<story>Facts(createDataset({ seed: 1, now: NOW, traces, profile: <STORY>_STORY_PROFILE }))` (support 5,000, the others 2,000); the filter, run through the mock's trace list handler for that project, returns the traces the fact counts (support: exactly 41).

**Commit:** `Ask each story's question to the assistant`

## P4 Issues and alerts

**Edit:** `web/src/mocks/generated/stories.ts`, `stories.test.ts`. Survey § 3 and § 6.

1. Issues (ids stay). `StoryIssue.hero` becomes `{ index: (dataset: Dataset) => number; spanId?: (dataset: Dataset, index: number) => string | undefined; reason: string }` (the support/coding/voice/extraction showcases are no longer the heroes); `storyIssue`, `storyOccurrences`, `storyQueueItem` and the tests call them. Replace each issue's `sourceName`, `kind`, `title`, `description`, `hero` and `matches`:

| Story | Source name | Kind | Title | Matches | Hero index | Hero span | Reason (the example's link text) |
|---|---|---|---|---|---|---|---|
| Support | Topics | topic | Customers asking for SSO on the Starter plan | `input_preview === SSO_TURN.question` | first matching visible trace by index | none | Do you have SSO on the Starter plan? |
| Coding | Errors | error | ContextWindowExceeded: diff over 23 files | tag `context-window` | first matching | its first error span named `write patch` | The diff was too large for the model's context. |
| Voice | Bad scores | score | Agent talked over the caller | tag `interrupted` | first matching | none | The interruption check scored 0.3. |
| Extraction | Errors | error | Scanned PDF: OCR returned no text | tag `scanned` | first matching | its error span named `ocr` | The PDF is a scanned image with no text layer. |

   Descriptions: "Customers ask whether SSO comes with the Starter plan.", "Runs stop when the diff no longer fits the model's context.", "The agent starts speaking before the caller has finished.", "Scanned invoices give OCR nothing to read, so no fields are extracted."
2. Alert rules (ids stay):
   - `StoryRule.lastValue`, `lastCount` and `changedAgoMs` become functions of `(dataset, now)`. Hourly buckets are UTC hours ending at `now` (the alert preview's buckets).
   - Support 570201: name "Answer correctness under 0.7", condition score `correctness` avg, window `1h`, `lt 0.7`, `min_count 10`, no filter; state firing; `lastValue` the mean `correctness` of the traces started in `[now − 1 h, now)`, `lastCount` their number; `changedAgoMs` = `now` minus the start of the first hourly bucket whose mean is under 0.7 (about 5 h).
   - Coding 570202: name "Spend over $40 an hour", condition `cost`, window `1h`, `gt 40`, no filter (no `min_count`); firing; `lastValue` the cost of `[now − 1 h, now)`, `lastCount` its runs; `changedAgoMs` = `now − (now − 14 h)`; channel Team Slack.
   - Voice 570203: name "Slow replies on the voice desk", condition `latency_p95` target trace, window `15m`, `gt 1500`, `min_count 20`, no filter; firing (the line stays above the threshold all day: the design asks for firing, not a rise); `lastValue` the p95 duration in ms of `[now − 15 min, now)`, `lastCount` its turns (22 at the clock); `changedAgoMs` 6 h; channel **On-call pager** `…000503` (P5 serves it).
   - Extraction 570204 stays as it is (M19 changes it).
   - `StoryRule` gets `channel: { id: string; name: string; kind: "slack" | "webhook" }` (Team Slack `…000500` slack; On-call pager `…000503` webhook); `storyAlertRule` uses its id and `storyAlertEvent`'s delivery its name and kind, instead of the hard-coded Team Slack.
3. Tests: `stories.test.ts`'s `datasetOf` builds the **story profiles** (`SUPPORT_STORY_PROFILE` …) at the screenshot clock `2026-11-19T12:00:00Z` (traces: support 5,000, others 2,000), and the tests that read `hero.index` call it with the dataset. Each issue's count and first example come from that dataset; the support issue's count over `[now − 7 d, now)` is `ssoThisWeek` and over `[now − 14 d, now − 7 d)` `ssoPreviousWeek` (grouping by UTC day doesn't change these windows' totals: count by `start_time`); the support issue's last-7-days count is `ssoThisWeek` and the previous 7 days `ssoPreviousWeek`; each rule's `lastValue` equals the value recomputed from the dataset; the voice rule's channel is `…000503`.

**Commit:** `Point each story's issue and alert at its own data`

## P5 Queue, check and judge

**Edit:** new `generated/story-evaluate.ts`, new `story-evaluate.test.ts`, `generated/handlers.ts` (register the new handlers in `largeHandlers`, after `storyHandlers`, with the same project prefix), `generated/stories.ts` (the support queue). Survey § 4.1-4.3 and § 6.3.

1. **Channel:** `storyChannelHandlers(project?)` serves `GET channels` for `acme/voice-desk` as the fixture channels plus one `NotificationChannel` (openapi) copied from the fixture "Pager webhook" (`fixtures/api/static/channels.json`) with `id: "…000503"`, `name: "On-call pager"`, its URL target `https://hooks.example.com/on-call`, and no `last_delivery`; every other field as the Pager webhook's. Check the result against the `NotificationChannel` schema.
2. **Support queue** (`stories.ts`, queue 570501): `target: "session"`; `StoryQueue.heroIndex` becomes optional and the queue gets `sessionId: "sess-0142"`; its item has `session_id: "sess-0142"`, no `trace_id`, and `target_time` = the session's first turn's `start_time`; `storyQueueItem` and the `?target_trace_id` list handler skip a session queue for trace lookups. `definitionIds` [correctness 611, tone 615, policy `…570607`]; counts `{ completed: 3, skipped: 1, pending: 21 }` ("4 of 25 done"); name "Answers with a thumbs-down", instructions "Read the whole conversation. Grade the answer that got the thumbs-down: is it correct, is the tone right, does it follow the policy?". Add `policy` (`…570607`, boolean, direction higher) to `STORY_SCORE_DEFINITIONS`, and also `total` `…570608`, `vat_number` `…570609`, `vendor` `…570610`, `date` `…570611` (boolean, higher) for P6's badges. Update the tests this breaks: `stories.test.ts` (the queue's hero equals the issue's hero: support now has none; the definition-id lists; `progress` support `4/25`) and `handlers.test.ts` (lines ~275-310 pin the score-definition names and ids: add the five new definitions in order; line ~333: queue 570501's review fields are correctness, tone, policy).
3. **Coding check and online rule** (survey § 4.2 is about evaluators; the online rule is the `?rule=<id>` drawer of `web/src/routes/$org/$project/online-rules/index.tsx`, `RulePanel`, with `section[aria-label="Scores, last 24 hours"]` in `rule-detail.tsx`): an evaluator `…570901` "Tests pass after the patch" (`kind: "check"`, `config: { check: "regex", pattern: "\\d+ passed" }`, `output_score { name: "tests_pass", data_type: "boolean" }`, other fields as the fixture evaluator `…501`), and an online rule `…570911` "Tests pass on every run" on it (fields as `online-rule.json`, sampling 1, no filter). Serve under `acme/code-agent`: `GET evaluators` and `GET evaluators/:id`, `GET online-rules` and `GET online-rules/:id` (the story objects only, P-4), and `GET online-rules/stats` = `{ window: { from: now − 24 h, to: now } (RFC 3339 strings, as the fixture), data: [ { rule_id, jobs_hourly: 24 × { time, done, failed: 0, skipped: 0 }, scores: { data_type: "boolean", count, pass_rate, hourly: 24 × { time, value } } } ] }` (`OnlineRuleStatsResponse`; `hourly[i].time` the start of the hour `now − (24 − i) h`, `value` the mean `tests_pass` of the coding runs that started in it, `done` their number; `count` and `pass_rate` over the 24 hours). Also serve `GET online-rules/:id/backfills` (empty), `GET eval-jobs` filtered by the rule (read the query key the drawer sends, `rule_id` in `web/src/api/queries-online-rules.ts`) (empty) and `GET scores?…` for the rule (empty) so the drawer shows no fixture rows.
4. **Voice judge:** an evaluator `…570902` "Reads the booking back" (`kind: "llm_judge"`, `config: { connection_id: "…000400", model: "gpt-4.1-mini", prompt: "Did the agent read the booking back to the caller in one sentence (day, time, doctor)? Caller: {{input}}\nAgent: {{output}}\nAnswer true or false, then why.", output: { type: "boolean" }, include_reason: true, temperature: 0, max_output_tokens: 200, timeout_ms: 20000 }`, `output_score { name: "readback", data_type: "boolean" }`) under `acme/voice-desk` (`GET evaluators`, `GET evaluators/:id`). `POST evaluators/test` under `acme/voice-desk` (a project-scoped `http.post` registered through `largeHandlers`, so it answers before `writes.ts`' fixture) returns `{ ok: true, value: { data_type: "boolean", value: false }, reason: "The agent offered 9:30 on Friday but never read the booking back before ending the call.", prompt: <the prompt with {{input}} = "Hi, can I move my appointment to Friday morning?" and {{output}} = "Friday at 9:30 is free. Should I book it?">, variables: { input: …, output: … }, usage: { input_tokens: 412, output_tokens: 38, cost_usd: 0.00023 }, latency_ms: 840, trace_truncated: false }` (no `judge_trace_id`).
5. Tests: each route answers in its project only (P-4); the stats' hourly values equal the dataset's; the queue item resolves to `sess-0142` with "4 of 25 done"; the judge's test answer has the reason above.

**Commit:** `Serve each story's queue, check and judge`

## P6 Invoice experiment

**Edit:** `generated/story-evaluate.ts`, `story-evaluate.test.ts`, `generated/experiments.ts` (scope `experimentHandlers()` to the support project ref, M16's note). Survey § 4.4.

1. Export from `experiments.ts` what a second set of runs needs: `type Run`, `compareBody`, `summaryOf`, and make the item count a field of the run spec (the support runs keep 30). `experimentHandlers(project?: string)` serves the fixture runs only under `acme/support-bot` (or unscoped); `handlers.ts`' call passes the project; `experiments.test.ts` follows.
2. Two runs over 300 shared items (`dataset_item_id` = `itemId` of the same offset for both, dataset "Labeled invoices" `…570921`, version 1): "extract-invoice v3" (`…570931`, metadata `{ prompt: "extract-invoice", version: 3, model: "gemini-2.5-flash" }`) and "extract-invoice v4" (`…570932`, version 4), `status: "completed"`, created 2026-11-18T10:00Z and 2026-11-18T11:00Z. Per row, boolean scores `total`, `vat_number`, `vendor`, `date` by rank: order the 300 rows by `mix(1, row)`; v3 passes `total` on the first 291, v4 on the first 297; v3 passes `vat_number` on the first 273, v4 on the first 288 (a second order, `mix(2, row)`); `vendor` and `date` pass on every row. Per row `cost_total` 0.0021 (v3) and 0.0024 (v4), latency 2,100 and 2,300 ms.
3. Serve under `acme/invoice-pipeline` only: `GET experiments` (the two), `GET experiments/:id`, `GET experiments/:id/results`, `GET experiments/compare?ids=` (built with `compareBody`). No dataset routes (the compare page doesn't read them).
   Unscoped (no project), the invoice runs answer by id and compare only; the list is the generated one.
4. Tests: the compare summary's pass rates are total 97% → 99%, vat_number 91% → 96%, vendor and date 100%; the cost row 0.63 → 0.72 USD; `n = 300`; served only under the invoice project; the support fixture runs still answer under `acme/support-bot`.

**Commit:** `Compare two extraction prompts on labeled invoices`

## P7 Prompt versions

**Edit:** `web/src/mocks/stories-prompts.ts`, `stories-prompts.test.ts`, `web/src/mocks/playground.ts`, `playground.test.ts`. Survey § 5.

1. New versions (each the previous version's template plus one line, the new line last in the system message): `write-patch` v4 adds "Split changes over 20 files into steps."; `voice-reply` v5 adds "Read the booking back in one sentence: day, time and doctor."; `extract-invoice` v6 adds "Amounts may use comma decimals (1.234,56 means 1234.56)." Commit messages: "Split large refactors into steps", "Read the booking back", "Accept comma decimals". `production` label moves to the new version. Each new version copies the previous version's `model`, `temperature`, `maxTokens` and `variables`, with `at` one day after the previous version's `at` (all before 2026-11-19).
2. `playground.ts`: `answerOf` reads `source.prompt.version`; new answers: write-patch v4 → "This touches 23 files, so I'll do it in three steps:\n\n1. Rename `Project.owner` in the models and migrations.\n2. Update the API handlers and their tests.\n3. Update the web client and run the full suite." (with tokens 2,140/96, cost $0.0079, latency 2,900 ms); voice-reply v5 → "Done: Friday at 9:30, with Dr. Martin." (tokens 310/14, $0.0002, 620 ms, ttft 180 ms); extract-invoice v6 → the JSON `{ "vendor": "Nordic Freight", "invoice_number": "INV-2026-0521", "total": 1234.56, "currency": "EUR" }` (tokens 1,180/48, $0.0005, 1,400 ms). Older versions keep their answers; `source.prompt.version` is a number; v6 answers the same whatever the model. Each new answer's `trace_id`: write-patch v4 the first coding run tagged `context-window` (by index, at the screenshot clock), voice-reply v5 `VOICE_AUDIO_TRACE_ID`, extract-invoice v6 the first Nordic Freight trace tagged `re-extracted`; compute them from the story datasets in the test and pin them as constants in `playground.ts`.
3. Tests: each new version exists with its line; the playground answers per version.

**Commit:** `Add the prompt versions that fix each story`

## P8 Screenshots and texts

**Edit:** `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, PNGs under `website/src/assets/screenshots/`.

1. Check first that the session page redesign is on main and merged into this branch (design § 13's last rule); if not, stop and report. Entries for the 21 pictures (names from P1's bases; the 3 M19 pictures keep their M16 entries under the new names):

| Shot | Path (after `/acme/<project>`) | Actions | waitFor / waitAfter | Viewport, clip |
|---|---|---|---|---|
| marketing-trace-page | `/sessions/sess-0142` | — | waitFor "Can I schedule it every week?" | 900×900, `section[aria-label="Conversation"]` |
| marketing-trace-page-coding | `/traces?trace=a0000000000000000000000000000004&mode=transcript` | — | waitFor "48 passed" | 1400×900, `aside[aria-label="Trace"]` |
| marketing-trace-page-voice | `/traces?trace=<VOICE_AUDIO_TRACE_ID>&span=<VOICE_AUDIO_SPAN_ID>` | — | waitFor "Friday at 9:30 is free" | 1400×900, `aside[aria-label="Trace"]` |
| marketing-assistant[-story] | `/traces` | click `button[aria-label="Ask AI"]`, click `button[aria-label^="Chats:"]`, click the menu item with the question | waitFor "loaded", waitAfter "Open in traces" | as today (1024×1000, window rect, clipRect 900×1000) |
| marketing-issue[-story] | `/issues/<id>` | — | waitFor the issue's title | 800×600, `main` |
| marketing-evaluate | `/annotation-queues/…570501/review/…570511` | click `section[aria-label="Conversation"] [data-index="2"]` | waitFor "4 of 25 done" | 1400×900, `main` |
| marketing-evaluate-coding | `/online-rules?rule=…570911` | — (an element clip scrolls itself) | waitFor "Tests pass on every run" | 900×700, `section[aria-label="Scores, last 24 hours"]` |
| marketing-evaluate-voice | `/evaluators/…570902?tab=config` | click `#test-trace-id`, type `VOICE_AUDIO_TRACE_ID`, click `button:has-text("Run test")` | waitFor "Reads the booking back", waitAfter "never read the booking back" | 900×1000, `main` |
| marketing-evaluate-extraction | `/experiments/compare?ids=…570931,…570932` | — | waitFor "vat_number" | 900×700, `main` |
| marketing-playground-coding | `/playground?prompt=write-patch&version=4` | type the variables (`issue`: "Rename Project.owner to Project.admin across the codebase (issue #409).", `path`: "src/", `code`: "class Project:\n    owner: User"), click "Run all" | waitFor "Run all", waitAfter "three steps" | 960×640, `main` |
| marketing-playground-voice | `/playground?prompt=voice-reply&version=5` | type the variables as today's v4 shot, with the caller asking to move the appointment to Friday at 9:30 with Dr. Martin; click "Run all" | waitFor "Run all", waitAfter "Done: Friday at 9:30" | 960×640, `main` |
| marketing-playground-extraction | `/playground?prompt=extract-invoice&version=6` | type `ocr_text`: "NORDIC FREIGHT AB … INV-2026-0521 … TOTAL 1.234,56 EUR"; click "Run all" | waitFor "Run all", waitAfter "1234.56" | 960×640, `main` |
| marketing-alerts[-coding\|-voice] | `/alerts?rule=<id>` | click `button[aria-label="Bottom"]` | waitFor the rule's name, waitAfter "History" | 900×900, `main` |

   Every entry has `"theme": "dark"` like today's marketing entries, and the only actions are the script's `click` and `type` (`website/scripts/screenshots.mjs`). The issue ids are 570101-570104 and the rule ids 570201-570203 as today; read the exact variable names of each prompt version in `stories-prompts.ts`. Where an action or a selector doesn't work, stop and report rather than change the UI.
2. Take them: `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots <names>`.
3. Look at each PNG and write its alt and caption in `stories.mjs`, and update `stories.test.mjs`' `EXPECTED` captions to match (the test's rules: one sentence, alt ≤ 260 characters, caption ≤ 100, distinct, `website/STYLE.md`). Numbers quote the facts as the pictures show them.
4. `docs/plans/website/README.md`: M18 done.

**Commit:** `Take the story pictures`

## P9 Docs shots

Run `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots <the docs shot names>` (the names in `screenshots.json` without the `marketing-` prefix); for each docs PNG that changed, check its alt in the docs page that embeds it and fix what no longer matches; `just check-docs`. Commit `Retake the docs shots after the story pictures` (only if something changed).

## After P9 (the coordinator)

A Sonnet code review of M18, fixes, `just check-web` and `just check-docs`, push.
