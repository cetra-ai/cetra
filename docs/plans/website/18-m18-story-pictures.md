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
| Answers 2026-11-25 | Issues show the kind through the source's name; voice judge on the Configuration tab with a test result; coding Evaluate on the online rule page's hourly line; the assistant's actions are the Filter card's "Open in traces" and the Dashboard card's "Open in dashboard editor"; the coding cost step is inside the chart's 24 hours (design § 13) |
| P-1 (planner) | Story objects stay in `stories.ts`, `stories-prompts.ts`, `assistant.ts` and `playground.ts`, and new kinds of story objects (evaluators, online rules, channels, the invoice dataset and experiments) go in one new module, `generated/story-evaluate.ts`, served by `storyHandlers` per project like the others (M16 P-4: only under the story's project). |
| P-2 (planner) | Texts quote M17's pinned facts as rounded numbers (62%, 41, 18%, 70%, ×2.7, 1.5 s, 3.5 s, 71%, 6%, 38%, 214, 30). A test per story module checks each quoted number against `facts(dataset)` at the screenshot clock, so a data change can't leave a text wrong. |
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
| Evaluate | Coding | the online rule page of the `tests_pass` check, with its hourly line | P5 |
| Evaluate | Voice | the LLM judge's Configuration tab with a test result | P5 |
| Evaluate | Extraction | `experiments/compare?ids=<v3>,<v4>` | P6 |
| Prompts | Coding, Voice, Extraction | the playground on the new version | P7 |
| Alert | Support, Coding, Voice | `/alerts?rule=<id>` | P4 |

## Phases

| Phase | Edit | Needs |
|---|---|---|
| P1 Switcher names | `website/src/marketing/data/stories.mjs`, `stories.test.mjs` | — |
| P2 Voice audio turn | `web/src/mocks/generated/story-voice.ts`, `story-voice.test.ts` | — |
| P3 Assistant chats | `web/src/mocks/assistant.ts`, `assistant.test.ts` | — |
| P4 Issues and alerts | `web/src/mocks/generated/stories.ts`, `stories.test.ts` | — |
| P5 Queue, check and judge | new `web/src/mocks/generated/story-evaluate.ts`, new `story-evaluate.test.ts`, `web/src/mocks/generated/handlers.ts` (registration only), `web/src/mocks/generated/stories.ts` (the support queue only) | P4 |
| P6 Invoice experiment | `web/src/mocks/generated/story-evaluate.ts`, `story-evaluate.test.ts`, `web/src/mocks/generated/experiments.ts` | P5 |
| P7 Prompt versions | `web/src/mocks/stories-prompts.ts`, `stories-prompts.test.ts`, `web/src/mocks/playground.ts`, `playground.test.ts` | — |
| P8 Screenshots and texts | `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, PNGs | P1-P7 |
| P9 Docs shots | PNGs and alts of docs pages that changed | P8 |

P1, P2, P3, P4 and P7 touch disjoint files and can run in parallel in one worktree, committing one after another (`git commit -- <files>`; on `index.lock`, wait and retry). P5 waits for P4 (both edit `stories.ts`), P6 for P5.

**Checks in every phase:** `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`; web phases from `web/`: `node_modules/.bin/vitest run <the phase's test files> --maxWorkers=4`, `pnpm typecheck`, `../node_modules/.bin/biome check --write <files>` then without `--write`; website phases: `cd website && node --test src/marketing/data/stories.test.mjs` and `just check-docs`. Never the full suite or `just check`. Delete `web/.vitest/` if created.

---

## P1 Switcher names

**Edit:** `website/src/marketing/data/stories.mjs`, `website/src/marketing/data/stories.test.mjs`.

1. The `trace` picture: `section` "Trace", `base` "marketing-trace-page", `label` per story "Session · Support bot", "Trace · Code agent", "Trace · Voice desk", "Traces · Invoice pipeline".
2. The `evaluate` picture: `section` "Evaluate", `base` "marketing-evaluate", `label` per story "Annotation queue", "Online rule", "LLM judge", "Experiments".
3. The `explore` picture keeps "Assistant"; `issues`, `prompts`, `alert` keep theirs.
4. Alts and captions stay as they are in P1 (P8 rewrites them); the shot names now point to `marketing-trace-page[-<story>]` and `marketing-evaluate[-<story>]`, which don't exist yet: P1 also renames the existing entries in `website/screenshots.json` and the PNG files (`git mv`) so the check passes: `marketing-trace-list*` → `marketing-trace-page*`, `marketing-queue-review*` → `marketing-evaluate*`. Add `website/screenshots.json` and the PNGs to this phase's commit.
5. `stories.test.mjs`: the shot-name test covers the new names.

**Commit:** `Name the story pictures whose page differs per story`

## P2 Voice audio turn

**Edit:** `web/src/mocks/generated/story-voice.ts`, `story-voice.test.ts`. Survey § 1.3.

1. Pick the turn: the first eligible voice turn by index that is tagged `calendar`, has a marked `check_availability` span, is not the hero, and starts within the last 2 hours; export its index as `VOICE_AUDIO_TURN` (computed by a function of the dataset, `voiceAudioTurn(dataset)`, so it follows the data).
2. Its mark gets a marked `llm` span `{ name: "draft reply", fromMs: d + 900, durationMs: 700 }` (d the turn's `check_availability` duration) whose `input` is messages: a `user` message with an audio part `{ type: "uri", uri: <CALLER_WAV>, modality: "audio", mime_type: "audio/wav", transcript: "Hi, can I move my appointment to Friday morning?" }`, an `assistant` message with a `tool_call` part `{ type: "tool_call", id: "call_1", name: "check_availability", arguments: { day: "Friday", time: "morning" } }`, and a `tool` message with a `tool_call_response` part `{ type: "tool_call_response", id: "call_1", response: { slots: ["9:30", "11:00"] } }`; its `output` is an `assistant` message with an audio part `{ type: "uri", uri: <AGENT_WAV>, modality: "audio", mime_type: "audio/wav", transcript: "Friday at 9:30 is free. Should I book it?" }`. The part shapes are spec 01's (`crates/server/openapi.json` `UriPart`, `ToolCallPart`, `ToolCallResponsePart`): check each field name there before writing it.
3. `CALLER_WAV` and `AGENT_WAV`: `data:audio/wav;base64,…` URIs built at module load by a function `toneWav(seconds, seed)` that writes a 16-bit mono PCM WAV at 8,000 Hz (RIFF header, then samples of a few summed sines with a slow amplitude envelope, so the waveform has visible shape): 2.4 s for the caller, 2.0 s for the agent. Write the header bytes with a `DataView`; base64 with `btoa` over the byte string.
4. Tests: `voiceAudioTurn` returns a turn of the last 2 hours with a `calendar` tag; its `draft reply` span's detail has the four parts above with their transcripts; each audio URI decodes to a RIFF/WAVE header of the stated length.

**Commit:** `Give a voice turn its call audio`

## P3 Assistant chats

**Edit:** `web/src/mocks/assistant.ts`, `assistant.test.ts`. Survey § 2.

1. `TurnSpec.draft` becomes `{ kind: "dashboard" | "filter"; draft: Fields }`; `storyEvents` and `storyMessages` emit a `filter` draft as they do a dashboard one (the default script, lines 64-96 and 186-200, shows the filter draft's shape).
2. Each story's chat becomes **one turn** (remove the dashboard turn): the question, a `query_sql` call whose arguments are the SQL below and whose result has the rows' summary, then the answer, then a `filter` draft `{ view: "traces", filter, range, include_eval: false, include_experiments: false }`. The answer quotes the facts (P-2):

| Story | Question | SQL (call argument) | Answer | Filter draft |
|---|---|---|---|---|
| Support | Why did our thumbs-down rate double since Tuesday? | `SELECT release, count(*) FILTER (WHERE has_score('user_feedback', false)) AS downvotes FROM traces WHERE start_time >= now() - INTERVAL '46 hours' GROUP BY release` | **62%** of the downvoted answers since Tuesday 14:00 quote the old pricing page: retrieval returns `pricing-2025.md`, back in the index since deploy v2.14. That's **41** traces. | `release = 'v2.14' and tags @> ['old-pricing']`, range `2d` |
| Coding | Why did cost per run triple yesterday? | `SELECT count(*) FILTER (WHERE run_tests > 5) AS retried, sum(cost_total) AS cost FROM runs WHERE start_time >= now() - INTERVAL '14 hours'` | **18%** of runs call `run_tests` more than five times and make **70%** of the cost: they all touch `packages/api`, whose test suite times out at 30 s, so the agent retries. | `tags @> ['api-retry']`, range `1d` |
| Voice | Why do callers hang up? | `SELECT … FROM traces WHERE tags @> ['hangup']` (write the full query in the same style) | **71%** of hang-ups follow a turn over 2.5 s, almost all of them `check_availability` on the calendar API (median **1.5 s**, p95 **3.5 s**). | `tags @> ['calendar'] and duration_ns > 2500000000`, range `1d` |
| Extraction | Which vendors' invoices fail the most, and why? | `SELECT vendor, avg(re_extracted) FROM invoices GROUP BY vendor ORDER BY 2 DESC` (same style) | Nordic Freight: **38%** re-extracted, all on `total`. It writes amounts as "1.234,56" and the schema expects a dot; Baltic Paper, Rhine Tools and Lyon Bureau use the same format. | `tags @> ['re-extracted']`, range `7d` |

   The numbers in bold are rounded from the facts (`quotingShare`, `quotingOldPricing`, `retryShare`, `retryCostShare`, `hangupsAfterSlowTurn`, `availabilityMedianMs`, `availabilityP95Ms`, `nordicFreightRetryShare`). Check every filter against the mock's filter columns (survey § 1.4: tags, release, duration_ns are allowed).
3. Remove what only the dashboard turns used (`dashboardTurn`, the drafts built from `storyDashboards()`), unless something else uses it; the story dashboards themselves stay (other pictures and tests use them).
4. Tests: per story, the stored chat has the question, a `query_sql` step, the answer and a `filter` draft; **each bold number** equals the rounded fact from `<story>Facts(createDataset({ seed: 1, now: NOW, traces, profile: <STORY>_STORY_PROFILE }))` (support 5,000, the others 2,000); the filter, run through the mock's trace list handler for that project, returns the traces the fact counts (support: exactly 41).

**Commit:** `Ask each story's question to the assistant`

## P4 Issues and alerts

**Edit:** `web/src/mocks/generated/stories.ts`, `stories.test.ts`. Survey § 3 and § 6.

1. Issues (ids stay): replace each `StoryIssue`'s `sourceName`, `kind`, `title`, `description`, `hero` and `matches`:

| Story | Source name | Kind | Title | Matches | Hero |
|---|---|---|---|---|---|
| Support | Topics | topic | Customers asking for SSO on the Starter plan | `input_preview === SSO_TURN.question` | the first matching trace by index (no hand-written hero: give `hero` an index computed from the dataset, as P2 does) |
| Coding | Errors | error | ContextWindowExceeded: diff over `<n>` files | tag `context-window` | first matching, `spanId` of its `write patch` error span |
| Voice | Bad scores | score | Agent talked over the caller | tag `interrupted` | first matching |
| Extraction | Errors | error | Scanned PDF: OCR returned no text | tag `scanned` | first matching, `spanId` of its `ocr` span |

   `hero.index` becomes a function of the dataset where the issue has no showcase (the current field is a constant: change the type to `index: number | ((dataset: Dataset) => number)`, or compute it in `storyIssue`; pick the smaller change and say which). Descriptions: one sentence each, plain words, no numbers.
2. Alert rules (ids stay):
   - Support 570201: name "Answer correctness under 0.7", condition score `correctness` avg, window `1h`, `lt 0.7`, `min_count 10`, no filter; state firing; `lastValue` = the mean correctness of the last hour (computed: replace the constant with a function of the dataset, as for the issues); `changedAgoMs` the time since the dip (`now − (now − 5 h)` rounded to the hour after the first hourly mean under 0.7).
   - Coding 570202: name "Spend over $40 an hour", condition `cost`, window `1h`, `gt 40`, no filter (no `min_count`); firing; `lastValue` the last whole hour's cost (45); channel Team Slack.
   - Voice 570203: name "Slow replies on the voice desk", condition `latency_p95` target trace, window `15m`, `gt 1500`, `min_count 20`; firing; `lastValue` the last 15 minutes' p95; channel **On-call** (P5 adds it; until then reference its id `…000503`).
   - Extraction 570204 stays as it is (M19 changes it).
   - `storyAlertRule` and `storyAlertEvent` take the channel per rule (a new `channelId` field on `StoryRule`; `storyAlertEvent`'s delivery names that channel) instead of the hard-coded Team Slack.
3. Tests: each issue's count and first example come from the story dataset; the support issue's last-7-days count is `ssoThisWeek` and the previous 7 days `ssoPreviousWeek`; each rule's `lastValue` equals the value recomputed from the dataset; the voice rule's channel is `…000503`.

**Commit:** `Point each story's issue and alert at its own data`

## P5 Queue, check and judge

**Edit:** new `generated/story-evaluate.ts`, new `story-evaluate.test.ts`, `generated/handlers.ts` (register the new handlers in `largeHandlers`, after `storyHandlers`, with the same project prefix), `generated/stories.ts` (the support queue). Survey § 4.1-4.3 and § 6.3.

1. **Channel:** `storyChannelHandlers(project)` serves `GET channels` for `acme/voice-desk` as the fixture channels plus `{ id: "…000503", kind: "webhook", name: "On-call", … }` (copy the Pager webhook's shape).
2. **Support queue** (`stories.ts`, queue 570501): `target: "session"`, the item's `session_id` `sess-0142`; `definitionIds` [correctness 611, tone 615, policy `…570607`]; counts `{ completed: 3, skipped: 1, pending: 21 }` ("4 of 25 done"); name "Answers with a thumbs-down", instructions "Read the whole conversation. Grade the answer that got the thumbs-down: is it correct, is the tone right, does it follow the policy?". Add the `policy` definition (`…570607`, boolean, direction higher, name `policy`) to `STORY_SCORE_DEFINITIONS`. `storyQueue`/`storyQueueItem` get a `target` and a `sessionId` (survey § 4.1).
3. **Coding check and online rule:** an evaluator `…570901` "Tests pass after the patch" (`kind: "check"`, `config: { check: "regex", pattern: "\\d+ passed", target: "output" }` in the fixture's check shape, `output_score { name: "tests_pass", data_type: "boolean" }`), an online rule `…570911` "Tests pass on every run" on it (filter none, sampling 1), served under `acme/code-agent` for the evaluator list, the evaluator, the online rule list and the online rule. The rule page's hourly line reads its stats (find the endpoint in survey § 4.2 and `online-rule-stats.json`); serve stats computed from the coding dataset's `tests_pass` scores per hour over the last 24 h (the M17 fact `testsPassRateSince` / `testsPassRateBefore` show the dip at `now − 14 h`).
4. **Voice judge:** an evaluator `…570902` "Reads the booking back" (`kind: "llm_judge"`, model `gpt-4.1-mini`, prompt "Did the agent read the booking back to the caller in one sentence (day, time, doctor)? Answer yes or no, then why.", output boolean, `include_reason: true`) under `acme/voice-desk`; `POST evaluators/test` for it answers `{ ok: true, value: false, reason: "The agent offered 9:30 on Friday but never read the booking back before ending the call.", usage: { input_tokens: 412, output_tokens: 38, cost_usd: 0.00023 }, latency_ms: 840, … }` in `EvaluatorTestResult`'s shape (other fields as the fixture), and the test runs on `voiceAudioTurn` (P2) when the panel asks for a trace.
5. Tests: each route answers in its project only; the stats' hourly values equal the dataset's; the queue item resolves to `sess-0142` with "4 of 25 done".

**Commit:** `Serve each story's queue, check and judge`

## P6 Invoice experiment

**Edit:** `generated/story-evaluate.ts`, `story-evaluate.test.ts`, `generated/experiments.ts` (scope `experimentHandlers()` to the support project ref, M16's note). Survey § 4.4.

1. A dataset "Labeled invoices" (`…570921`) of 300 items under `acme/invoice-pipeline`: item k's input is an invoice file name and vendor from the extraction turns (cycle), expected output its fields.
2. Two experiments, "extract-invoice v3" (`…570931`) and "extract-invoice v4" (`…570932`), each with 300 rows: per row boolean scores `total`, `vat_number`, `vendor`, `date`; deterministic by row (`mix`): v3 passes `total` on 291 rows (97%) and `vat_number` on 273 (91%), v4 on 297 (99%) and 288 (96%); `vendor` and `date` 100% both. Per-row `cost_total` 0.0021 (v3) and 0.0024 (v4), latency 2,100 and 2,300 ms.
3. Serve under `acme/invoice-pipeline`: `GET experiments`, `GET experiments/:id`, `GET experiments/compare?ids=` (the `ExperimentComparison` shape of `experiments.ts`'s `compareRoute`; reuse its builder by passing these runs rather than writing a second one), `GET datasets`, `GET datasets/:id`.
4. Tests: the compare summary shows the four pass rates and the cost row; `n = 300`; served only under the invoice project.

**Commit:** `Compare two extraction prompts on labeled invoices`

## P7 Prompt versions

**Edit:** `web/src/mocks/stories-prompts.ts`, `stories-prompts.test.ts`, `web/src/mocks/playground.ts`, `playground.test.ts`. Survey § 5.

1. New versions (each the previous version's template plus one line, the new line last in the system message): `write-patch` v4 adds "Split changes over 20 files into steps."; `voice-reply` v5 adds "Read the booking back in one sentence: day, time and doctor."; `extract-invoice` v6 adds "Amounts may use comma decimals (1.234,56 means 1234.56)." Commit messages: "Split large refactors into steps", "Read the booking back", "Accept comma decimals". `production` label moves to the new version.
2. `playground.ts`: `answerOf` reads `source.prompt.version`; new answers: write-patch v4 → "This touches 23 files, so I'll do it in three steps:\n\n1. Rename `Project.owner` in the models and migrations.\n2. Update the API handlers and their tests.\n3. Update the web client and run the full suite." (with tokens 2,140/96, cost $0.0079, latency 2,900 ms); voice-reply v5 → "Done: Friday at 9:30, with Dr. Martin." (tokens 310/14, $0.0002, 620 ms, ttft 180 ms); extract-invoice v6 → the JSON `{ "vendor": "Nordic Freight", "invoice_number": "INV-2026-0521", "total": 1234.56, "currency": "EUR" }` (tokens 1,180/48, $0.0005, 1,400 ms). Older versions keep their answers.
3. Tests: each new version exists with its line; the playground answers per version.

**Commit:** `Add the prompt versions that fix each story`

## P8 Screenshots and texts

**Edit:** `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, PNGs under `website/src/assets/screenshots/`.

1. Entries for the 21 pictures (names from P1's bases; the 3 M19 pictures keep their M16 entries under the new names), each with the page and state of § The pictures, a viewport whose CSS width is at most 900 px for the clipped area (the design's readability rule), and `waitFor`/`waitAfter` texts that exist only in that picture's data. The assistant shots: the project's traces page, `Ask AI`, `Chats:`, the menu item with the question; `waitAfter` "Open in traces"; window rect as today.
2. Take them: `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots <names>`.
3. Look at each PNG and write its alt and caption in `stories.mjs` (the test's rules: one sentence, alt ≤ 260 characters, caption ≤ 100, distinct, `website/STYLE.md`). Numbers quote the facts as the pictures show them.
4. `docs/plans/website/README.md`: M18 done.

**Commit:** `Take the story pictures`

## P9 Docs shots

Run `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots` for every shot; for each docs PNG that changed, check its alt in the docs page that embeds it and fix what no longer matches; `just check-docs`. Commit `Retake the docs shots after the story pictures` (only if something changed).

## After P9 (the coordinator)

A Sonnet code review of M18, fixes, `just check-web` and `just check-docs`, push.
