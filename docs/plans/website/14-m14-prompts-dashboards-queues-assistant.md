# M14: Prompts, dashboards, queues and the assistant

The homepage's four stories (Support agent, Coding agent, Voice agent, Document extraction; design §12, D-W-24, D-W-25) each get their own prompt and playground answer, dashboard, annotation queue and assistant conversation in the web mocks, tied to the story's hero trace. It also gives the generated voice-tagged traces plausible durations, so the voice story's dashboard shows believable latencies. It is six phases:
- P0 rescales the durations of generated voice traces.
- P1 adds the story prompts and the playground's story answers.
- P2 defines the four story dashboards.
- P3 adds the four story annotation queues and their score definitions to the large mock.
- P4 scripts the assistant's eight story turns and stores one two-turn chat per story.
- P5 registers P1's prompts and P2's dashboards in the browser worker, runs the web checks once and looks at the pages.

M15 takes the screenshots and builds the switcher.

**Prerequisites:** branch `claude/focused-screenshots` at or after the commit that adds this plan; M13 done (`docs/plans/website/13-m13-voice-issues-alerts.md`).

**Read first:**
- `docs/website/00-design.md` §12.
- `docs/plans/website/README.md` § Conventions for every phase. They apply here; this plan edits `web/` (and one README row), and each phase names its own checks.
- `docs/plans/website/13-m13-voice-issues-alerts.md`: S1 shows how story objects live next to the generated data (`web/src/mocks/generated/stories.ts`).
- `web/src/mocks/generated/dataset.ts`: `SHOWCASES`, `traceSummary`, `pickTags`, `createDataset`, `visibleTraces`.
- `web/src/mocks/generated/widget-query.ts` and `web/src/lib/dashboards/` (`types.ts`, `validate.ts`): what a widget query may say.
- `web/src/mocks/browser.ts`, `dashboards.ts`, `fallbacks.ts`, `playground.ts`, `assistant.ts`, `generated/handlers.ts`, `generated/stories.ts`.
- `fixtures/api/responses/prompts.json`, `fixtures/api/static/annotation-queues.json`.

## What it is, and what it is not

When it's done, in the dev and demo mock (large mode):
- **Prompts and playground.** `/prompts` lists `summarize` and `support-reply` (fixtures), then `write-patch`, `voice-reply` and `extract-invoice`. A playground run answers with the story's own text, tokens, cost and latency, and its "Open trace" opens the story's hero trace. The two extraction columns answer with different totals.
- **Dashboards.** Four dashboards, ids `01900000-0000-7000-8000-000000570401` to `…570404`, each with four widgets that the large mock answers with data.
- **Queues.** Four annotation queues, ids `…570501` to `…570504`, each with one pending item that is the story's hero trace, the counts and the score fields of the fixed content.
- **Assistant.** The story's question and its build request each get their scripted answer; the turns are stored in one conversation as they were streamed, and four stored two-turn chats are listed.
- **Durations.** Generated voice-tagged traces last 0.4 to 4 s; no other trace changes.

Out of scope:
- The chapter screenshots, the switcher and the alts (M15). M15 also types what the playground cannot prefill (see Planner's readings).
- Any change to `fixtures/api/`, to the Rust code, to `docs/` other than the README row and this plan, or to the website.

**No `CHANGELOG.md` line:** the product doesn't change. **No new dependency.**

While phases run in parallel, nobody runs full test suites: each phase runs only the tests it names (`pnpm --dir web exec vitest run <paths> --maxWorkers=8`) and typechecks; P5 runs `just check-web` once, deliberately, as the last phase. Never `just check`.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-12 | Screenshots come from the web UI on its MSW mocks |
| D-W-24 | The hero shows four example traces behind tabs; the voice agent replaced the research agent |
| D-W-25 | Every homepage picture follows one of the four stories, through mock objects tied to the story's hero trace |

### Planner's readings

Choices that the fixed content below does not state. They come from the phase drafts and are collected here so you can see them.

Whole plan:
- A story's objects are tied to its hero trace by id (dataset indices 3 to 6); there is no story mode.
- The checked fixtures under `fixtures/api/` are not edited.
- In `mock=fixtures` mode the story dashboards are listed too; that is accepted.
- Parallel phases share no file: only P5 edits `web/src/mocks/browser.ts`.
- The story dashboards' widget definitions are written twice, in P2 (`story-dashboards.ts`) and in P4 (the assistant's draft cards), because the two phases share no file. Their titles, ids and layouts are the same; P5 checks the titles with a grep. The measure `label`s differ in a few widgets (the card shows titles only).

P0 (durations):
- Trace 0 (60 s) and the showcases keep their durations, even when tagged `voice`.

P1 (prompts and playground):
- Story prompts live in a new browser-only module, `stories-prompts.ts`, not in `fallbacks.ts` (which is built around `promptsFixture.data`). `GET /prompts` lists the fixture prompts first, then the three story prompts, and honours `q`, `label` and `tag`.
- The run's answer is chosen from the request body: `source.prompt.name` picks the story; for `extract-invoice`, a `model` starting with `claude` gives the right total and any other model the wrong one. No source, an unknown name or an unreadable body keeps the generic "Thanks for your message…" answer.
- `support-reply` keeps its fixtures (v2 labelled `latest` and `staging`); only its run answer is new.
- Story runs return the story's hero trace id (the ids are literals in `playground.ts`, checked against `createDataset` by a test); the generic run keeps its constant.
- The invoice JSON carries `line_items: 3`, a number, as the hero trace's payload does.
- The earlier versions of each story prompt, their commit messages and dates, the descriptions and tags, the token counts, costs and latencies of each answer, and the prompt ids `…570702` to `…570704` are the draft's choices.
- The playground cannot show variable values prefilled (`draftFromPromptVersion` starts with none, and `screenshots.mjs` clears `localStorage` before each shot). For M15: type `product`/`language`/`question` (support-reply), `issue`/`path`/`code` (write-patch), `caller`/`slots` (voice-reply), `ocr_text` (extract-invoice: `NORTHWIND SUPPLIES\nInvoice INV-2026-0412 · 18 Nov 2026\nDesk lamp × 4 .......... 236.00\nOffice chair × 2 ....... 698.00\nMonitor arm × 2 ........ 350.00\nTotal ................ 1,284.00 EUR`), and type `claude-sonnet-4-5` into column 2's model ("Add model" copies the connection and leaves the model empty). `screenshots.mjs`'s `type` action uses `insertText`, so check multi-line values on one real run.

P2 (dashboards):
- A stored dashboard has no filter of its own, so each widget carries its story's tag filter in `query.filter`; the page's filter box stays empty.
- "Correctness by day" reads the scores source (`avg value_num`, `name = 'correctness'`) and has no tag filter, because scores have no tags.
- "Failed runs per day" is a `count` with `where: "status = 'error'"`; days without a failure show 0.
- The author, creation date, `h: 6` 2×2 layout, the measure labels and the dashboard descriptions are the draft's.

P3 (queues):
- The queue data lives in `generated/stories.ts`, which `largeHandlers` already spreads first.
- The queue ids `…570501` to `…570504`, their `description` texts, and the one pending, leased hero item per queue (though the counts say 17, 15, 29 and 52) are the draft's choices; story queues are listed before the fixture queue.
- The voice queue reuses the generated `tone` definition (`…000615`, neutral, friendly, formal, curt); there is no definition `…570604`.

P4 (assistant):
- The story's two turns happen in the one new conversation (`…1a02`), as typed turns do; the stored copies listed are `…570801` to `…570804`, each with both turns.
- A turn is recorded only when run in the new conversation; `POST …/conversations` empties it; every turn run in it is appended.
- A turn's script is selected by `message.trim().toLowerCase()` compared with the story questions; anything else runs today's script unchanged.
- The text before each tool call, the turn-2 answer ("Here is the draft. Open it in the editor to review and save it."), the search filters and ranges, the extra trace ids in the search rows, and the story chats' timestamps (2026-11-19, the shots' clock; the new chat keeps 2026-01-02) are the draft's. The search arguments are cosmetic.

P5 (registration):
- The prompt handlers go just before `fallbackHandlers`; the dashboards are seeded as `[...seedDashboards(), ...storyDashboards()]`.

## Versions

No new dependency.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| P0 Plausible durations for generated voice traces | `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts` | — |
| P1 Story prompts and playground answers | `web/src/mocks/stories-prompts.ts` (new), `web/src/mocks/stories-prompts.test.ts` (new), `web/src/mocks/playground.ts`, `web/src/mocks/playground.test.ts` (new), `web/src/mocks/fallbacks.ts` (one `export`) | — |
| P2 Story dashboards | `web/src/mocks/story-dashboards.ts` (new), `web/src/mocks/story-dashboards.test.ts` (new) | — |
| P3 Story annotation queues | `web/src/mocks/generated/stories.ts`, `web/src/mocks/generated/stories.test.ts`, `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts` | — |
| P4 Story assistant conversations | `web/src/mocks/assistant.ts`, `web/src/mocks/assistant.test.ts` (new) | — |
| P5 Register the story mocks | `web/src/mocks/browser.ts`, `docs/plans/website/README.md` (the M14 row) | P1, P2 (merged) |

**Parallelism.** P0, P1, P2, P3 and P4 run in parallel, each in its own git worktree and branch, cut from the same base commit. Their Edit lists don't overlap (checked file by file above: `dataset.ts` is P0's alone, `generated/stories.ts` and `generated/handlers.ts` are P3's alone, `assistant.ts` is P4's alone, `fallbacks.ts` and `playground.ts` are P1's alone). The one file two drafts first wanted, `web/src/mocks/browser.ts` (P1 registers its handlers, P2 seeds its dashboards), belongs to P5 alone. P5 runs after P1 and P2 are merged into the plan's branch (P0, P3 and P4 may be merged before or after; P5 should run on the branch with all five merged, because it runs `just check-web` once). The coordinator merges the branches one at a time, resolves conflicts, and runs the full checks once after P5.

No phase runs full test suites while others run; a phase runs only the tests it names, and compiles and type-checks once, right before its commit.

Setup for every phase, from the worktree root: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`. Checks use `pnpm --dir web exec vitest run <paths> --maxWorkers=8`, `pnpm --dir web run typecheck`, and `./node_modules/.bin/biome check --write <files>` then the same without `--write`. Biome's formatting and import order win over code quoted in this plan; behaviour and strings stay as quoted. Never run `just check`. Every phase also checks `rtk proxy grep -rn "000000570[4-9]" fixtures web/src`: nothing under `fixtures/`, and under `web/src` only the files of the M14 phases.

---

## The fixed content

Decided by the planner. Every phase uses exactly this; "the content" in the phases means this section.

Stories and their hero traces (web/src/mocks/generated/dataset.ts SHOWCASES; trace id = "a" + index in hex padded to 32 chars):
- Support agent: trace index 3 `support-agent` (a0000000000000000000000000000003), user-012, tag email, prompt support-reply v2 on span "draft answer". Question "How can I export my traces to a CSV file?"
- Coding agent: index 4 `coding-agent` (…0004), user-007, tag retry, status error (run_tests span 0000000400000009 "1 failed, 47 passed").
- Voice agent: index 5 `voice-agent` (…0005), user-019, tag voice, check_availability span 0000000500000005 took 1.22 s; reply started 2.8 s after the caller.
- Document extraction: index 6 `extract-invoice` (…0006), no user, tag billing; first "extract fields" (gemini-2.5-flash) read total 1,248.00 instead of 1,284.00; "re-extract fields" (claude-sonnet-4-5) got it right.

Ids: every new mock object id is `01900000-0000-7000-8000-0000005704NN` (dashboards), `…5705NN` (queues), `…5705 1N` items (queue items `…570511`–`…570514`), `…5706NN` (score definitions), `…5707NN` (prompts if ids are needed), `…5708NN` (assistant conversations), `…5709NN` (assistant messages/turns). N = 1 support, 2 coding, 3 voice, 4 extraction. Before M14, `rtk proxy grep -rn "000000570[4-9]" fixtures web/src` is empty.

### Dashboards (one per story; widget titles are shared with the assistant's draft cards)
| N | Name | Description | Filter | Widgets (title — what it shows) |
|---|---|---|---|---|
| 1 | Answer quality | Support answers: quality, volume and cost. | `tags @> ['email']` | "Correctness by day" — line, avg of score correctness (scores source, filtered by `name = 'correctness'`; scores have no tags, so this one widget has no tag filter); "Answers per day" — bar, count of traces; "Cost per answer" — number, avg cost_total; "Answers by model" — bar_h, count by models |
| 2 | Code agent health | Failed runs, cost and duration of the coding agent. | `tags @> ['retry']` | "Failed runs per day" — bar, count where status = 'error'; "Error rate" — number, rate where error_count > 0; "Cost per run" — line, avg cost_total; "p95 duration" — number, p95 duration_ns |
| 3 | Voice latency | How long callers wait for a reply. | `tags @> ['voice']` | "p95 reply time" — number, p95 duration_ns; "Reply time by day" — line, p95 duration_ns; "Calls over 3 s" — number, rate where duration_ns > 3000000000; "Calls per day" — bar, count |
| 4 | Invoice extraction | Invoices read, schema-check failures and cost. | `tags @> ['billing']` | "Invoices per day" — bar, count; "Schema check failures" — number, rate where status = 'error'; "Cost per invoice" — line, avg cost_total; "Invoices by model" — bar_h, count by models |
No heading widget: four widgets laid out 2×2 on the 60-column grid (w 30 each). The widget query must answer every widget in large mode (only supported columns/aggs; check web/src/mocks/generated/widget-query.ts).

### Annotation queues (one per story; item = the hero trace)
| N | Queue name | Instructions | Score fields (definitions) | Counts (pending/completed/skipped, total) |
|---|---|---|---|---|
| 1 | Support answers | Score each answer against the customer's question. Skip the ones without a question. | correctness (numeric 0–1, existing generated definition), relevance (boolean, existing) | 12/4/1, 17 |
| 2 | Patch review | Check that the patch fixes the issue and that the tests pass. | tests_pass (boolean, "Whether the tests pass after the patch."), verdict (categorical: approve, request changes, reject; "Your review of the patch.") | 6/9/0, 15 |
| 3 | Call review | Listen for whether the caller got what they asked for, and how the reply sounded. | resolved (boolean, "Whether the caller's request was handled."), tone (the generated categorical definition `…000615`: neutral, friendly, formal, curt) | 20/7/2, 29 |
| 4 | Invoice fields | Compare the extracted fields with the invoice. | schema_valid (boolean, "Whether the extracted invoice passes the schema check."), fields_correct (categorical: all, some, none; "How many fields match the invoice.") | 31/18/3, 52 |
New definitions get ids `…570601`, `…570602`, `…570603`, `…570605`, `…570606` (tests_pass, verdict, resolved, schema_valid, fields_correct; there is no `…570604`, the voice queue reuses the generated `tone`) and are served with the generated ones.

### Prompts and playground
| N | Prompt | Version shown | Model | Template | Playground answer |
|---|---|---|---|---|---|
| 1 | support-reply (fixture exists, v2) | 2 | gpt-4o (fixture) | fixture | "Export from the Traces page, or call the export endpoint for large ranges.\n\n- Filter the traces you need\n- Choose Export and then CSV\n- Use the API for ranges over 100,000 traces" |
| 2 | write-patch | 3 | claude-sonnet-4-5 | system "You are a coding agent. Write a unified diff that fixes the issue, and nothing else." user "Issue: {{ issue }}\n\nFile {{ path }}:\n{{ code }}" | the diff "```diff\n-    return value.replace('\"', '\"\"')\n+    escaped = value.replace('\"', '\"\"')\n+    return f'\"{escaped}\"' if needs_quotes(value) else escaped\n```" |
| 3 | voice-reply | 4 | gpt-4.1-mini | system "You are a receptionist on the phone. Answer in one or two short sentences." user "{{ caller }}\n\nFree slots: {{ slots }}" | "Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you?" |
| 4 | extract-invoice | 5 | gemini-2.5-flash | system "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total." user "{{ ocr_text }}" | two columns: gemini-2.5-flash answers JSON with total 1248 (wrong), claude-sonnet-4-5 answers total 1284 (right); different tokens, cost, latency |
Each new prompt: type chat, labels production and latest on the shown version, 3 to 5 versions with short commit messages, served by a browser-only module like fallbacks.ts, and listed by GET /prompts after the fixture prompts. The run answer is chosen from the request (prompt name, or the model for the extraction columns); unknown requests keep today's "Thanks for your message…" answer.

### Assistant (one stored conversation per story; first turn answers, second turn builds)
| N | Question (turn 1) | Steps | Answer | Build request (turn 2) | Draft |
|---|---|---|---|---|---|
| 1 | Which answers scored low today? | Searched traces (rows 6) | "Six answers scored under 0.5 today, most about refunds. The CSV export answer (support-agent) scored 0.9." | Build me a dashboard of answer quality. | dashboard "Answer quality" with the 4 widget titles above |
| 2 | Which traces failed in the last day? | Searched traces (rows 1) | "One: coding-agent (a0000000…0004). Its first run_tests call failed with 1 failed, 47 passed, and the second run passed." | Build me a dashboard for code-agent: failed runs, cost per run and p95 duration. | "Code agent health" |
| 3 | Which calls were slow today? | Searched traces (rows 12) | "Twelve calls took over 3 s. In voice-agent (a0000000…0005), check_availability alone took 1.22 s." | Build me a dashboard of voice latency. | "Voice latency" |
| 4 | Which invoices needed a retry? | Searched traces (rows 3) | "Three. In extract-invoice (a0000000…0006), the first model read the total as 1,248.00 instead of 1,284.00, and the retry fixed it." | Build me a dashboard of invoices, schema-check failures and cost. | "Invoice extraction" |
The answer text may name the trace id in full; every id named must exist. The turn handler picks the script from the message text (the turn-1 question or the turn-2 build request of a story; anything else keeps today's script). Stored messages must equal what the live turns showed, so the conversation re-renders with the real question.

### Planner update
- Generated traces tagged `voice` get plausible durations: phase P0 in dataset.ts rescales their duration to 400 + (d − 200) × 3,600 / 20,000 ms (0.4–4 s), computed from the same random draw, so no other trace changes.
- The voice dashboard's third widget is "Calls over 3 s" (rate where duration_ns > 3000000000), and the voice assistant draft card lists the same title.
- In `mock=fixtures` mode the story dashboards are listed too; that is accepted.
- The voice queue reuses the generated "tone" definition (neutral, friendly, formal, curt) instead of a new one; there is no definition `…570604`.
- Parallel phases must not share files: no phase except the final registration phase edits `web/src/mocks/browser.ts` (P5).

---

### P0 Plausible durations for generated voice traces

**Goal:** generated traces tagged `voice` last 0.4 to 4 s instead of 0.2 to 20 s, computed from the same random draw, so no other trace changes.

**Edit:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts`. No other file.

**What the code does today** (`traceSummary`, `dataset.ts`): a generated trace (not a showcase, whose summary `showcaseSummary` builds from its spans) draws, in this order, from one `mulberry32` stream: span count, errored, error count, environment, experiment, age, then `const durationMs = index === 0 ? 60_000 : 200 + Math.floor(rng() * 20_000);` (so 200 to 20,199 ms), then name, model, tokens, session, user, and last `const tags = pickTags(rng);`. `durationMs` is read only in the returned object (`end_time` and `duration_ns`), after the tags. The tags are therefore known only after the duration is drawn; the rescaling must happen after `pickTags`, and must not add a draw.

**Steps:**

1. Record the baseline before editing `dataset.ts`. Create a throwaway test file `web/src/mocks/generated/baseline.tmp.test.ts` that builds `createDataset({ seed: 1, now: Date.parse("2026-06-01T00:00:00.000Z"), traces: 60 })` and `console.log`s `JSON.stringify` of `[index, trace.duration_ns / 1_000_000, trace.tags.includes("voice")]` for the traces at indices 1 to 59 that are not showcases (`SHOWCASES` is exported from `dataset.ts`). Run it with `pnpm --dir web exec vitest run src/mocks/generated/baseline.tmp.test.ts`, note the output, and delete the file. It is never committed.
2. In `dataset.ts`, after `const TAGS = ...`, add:
   ```ts
   /** Generated voice calls last 0.4 to 4 s, not the 0.2 to 20 s of the other traces (M14). */
   const VOICE_MIN_MS = 400;
   const VOICE_SPAN_MS = 3_600;
   /** The drawn duration is `200 + floor(rng() * 20_000)`: 200 to 20,199 ms. */
   const DRAWN_MIN_MS = 200;
   const DRAWN_SPAN_MS = 20_000;
   ```
3. In `traceSummary`, rename the line `const durationMs = index === 0 ? 60_000 : 200 + Math.floor(rng() * 20_000);` to `const drawnMs = index === 0 ? 60_000 : DRAWN_MIN_MS + Math.floor(rng() * DRAWN_SPAN_MS);`. It stays on the same line position, so the draws keep their order.
4. Immediately after `const tags = pickTags(rng);`, add exactly:
   ```ts
   // A voice call is shorter: rescale the drawn duration, with no new random draw. Trace 0 keeps its 60 s.
   const durationMs =
     index !== 0 && tags.includes("voice")
       ? VOICE_MIN_MS + Math.floor(((drawnMs - DRAWN_MIN_MS) * VOICE_SPAN_MS) / DRAWN_SPAN_MS)
       : drawnMs;
   ```
   Nothing between the old and the new line reads the duration (`startMs` uses `ageMs`); the returned object's `end_time` and `duration_ns` now read `durationMs` as before. `drawnMs - 200` runs 0 to 19,999, so a voice duration is 400 to 3,999 ms.
5. Run `rtk proxy grep -n "durationMs\|drawnMs" web/src/mocks/generated/dataset.ts` and check every use of `durationMs` in `traceSummary` is after step 4's line.
6. Add the tests below to `dataset.test.ts`.

**Tests** (in `dataset.test.ts`, using its `NOW`, `createDataset` and `SHOWCASES`; a "generated" trace is one at an index other than 0 that is not a showcase index):
- `"gives every generated voice trace a duration between 0.4 and 4 s"`: `createDataset({ seed: 1, now: NOW, traces: 2000 })`; the generated traces whose `tags` include `voice` number more than 100; every one has `duration_ns / 1_000_000` an integer from 400 to 3,999.
- `"keeps the drawn duration range of the other generated traces"`: the generated traces without the `voice` tag all last 200 to 20,199 ms, and at least one lasts more than 4,000 ms.
- `"draws the same durations as before for the traces it does not rescale"`: with `traces: 60`, the non-voice generated traces at the indices recorded in step 1 have exactly the recorded durations (a literal list of `[index, ms]` in the test), and the voice-tagged ones equal `400 + floor((recordedMs - 200) * 3600 / 20000)` of their recorded duration (a literal list of `[index, recordedMs]`). Write both lists from the step 1 output; the lists must have at least 10 non-voice and 3 voice entries (if the first 59 indices give fewer, raise the throwaway run's `traces` to 200 and use the first entries).
- `"leaves trace 0 and the showcases alone"`: trace 0 lasts 60,000 ms; every showcase's trace lasts `showcase.spans[0].to` ms (the existing test at "SHOWCASES" already pins each; this one adds trace 0).
- The existing tests, including "makes the voice agent's availability check its slowest span" (3,100 ms), pass unchanged.

**Done when:**
- `pnpm --dir web exec vitest run src/mocks/generated/dataset.test.ts src/mocks/generated/datasets.test.ts src/mocks/generated/widget-query.test.ts src/mocks/generated/handlers.test.ts src/mocks/generated/stories.test.ts src/mocks/sse.test.ts 'src/routes/$org/$project/traces/index.test.tsx' src/test/quality-bar.test.tsx --maxWorkers=8` passes (these are every test that builds a dataset; a failure there is reported with `QUESTION:`, not patched around);
- `pnpm --dir web run typecheck` passes;
- `./node_modules/.bin/biome check --write web/src/mocks/generated/dataset.ts web/src/mocks/generated/dataset.test.ts`, then the same command without `--write`, passes;
- `web/src/mocks/generated/baseline.tmp.test.ts` does not exist and `git status` lists only the two files.

**Commit:** `Give generated voice traces plausible durations`

---

### P1 Story prompts and playground answers

**Goal:** the browser mock serves three story prompts (write-patch, voice-reply, extract-invoice) next to the fixture prompts, and a playground run answers with the story's own text, tokens, cost and latency.

**Verified against the code:**
- `web/src/mocks/browser.ts` registers, in this order: `fixtureStreamHandlers()`, `playgroundHandlers`, saved views, dashboards, `sqlHandlers`, `assistantHandlers()`, `writeHandlers`, `fallbackHandlers`, `handlers` (manifest fixtures). First match wins; a resolver that returns `undefined` falls through. `largeHandlers` (generated/handlers.ts) has no prompt route, so a handler in `browser.ts` is not shadowed. `GET …/prompts` today is answered by the manifest alone (the fixture list: `summarize`, `support-reply`, no filter support).
- `fallbacks.ts` answers prompts from `promptsFixture.data` only (`promptOf`), so it cannot serve a new prompt without a list entry in a generated fixture (Rust-generated, off limits). Hence a new module.
- The run POST is `POST */api/v1/projects/:project/playground/runs` (`web/src/mocks/playground.ts`), body `RunRequest` (`web/src/lib/playground.ts:80`): `connection_id`, `model`, `messages`, `variables`, `placeholders`, `render_variables`, `params?`, `tools?`, `tool_choice?`, `response_format?`, `source?`. `source` is `Draft["source"]`: `{ prompt: { name, version } }` when the draft came from `?prompt=&version=` (`draftFromPromptVersion` sets `source: { prompt: { name: version.name, version: version.version } }`), `{ span: { trace_id, span_id } }` when from a span, absent for a blank draft. `model` is the column's model text, or the connection's `default_model` (`gpt-5-mini`, from `fixtures/api/static/llm-connections.json`) when the column's model is empty.
- Each column posts its own request, so the two columns of extract-invoice are two POSTs that differ in `model`.
- `draftFromPromptVersion` sets `variables: {}`, so variable values start EMPTY. There is no URL parameter for them, and `website/scripts/screenshots.mjs` runs `localStorage.clear()` before each shot, so a stored draft cannot carry them either. Verdict: **the playground cannot show variable values prefilled; the screenshot phase must type them** (screenshots.mjs supports `click` then `type`). "Add model" copies the last column's connection and leaves its model EMPTY (`addColumn`, index.tsx:675), so the screenshot phase must also type `claude-sonnet-4-5` into column 2's model field (the first column's model is `gemini-2.5-flash` from the v5 config).
- Only one LLM connection exists (OpenAI); a column with an Anthropic or Gemini model id is accepted as free text, the mock never reaches a provider.

**Decisions this phase makes**

| ID | Decision | Why |
|---|---|---|
| P1-a | Prompts are served by a new browser-only module `web/src/mocks/stories-prompts.ts`, registered by P5 in `browser.ts` just before `fallbackHandlers` (this phase does not edit `browser.ts`). | `fallbacks.ts` is built around `promptsFixture.data`; a prompt outside that list needs its own list entry, version texts, labels and history, which would bend every function of `fallbacks.ts`. The new module returns `undefined` for every name that is not a story prompt, so `fallbacks.ts` and the fixtures keep answering their prompts, and fixture mode and large mode both get the prompts (no dataset needed). The only edit to `fallbacks.ts` is `export` on `metricsOf`, reused for the metrics tab. |
| P1-b | `GET …/prompts` is answered by the new module: the fixture prompts first (the order of `promptsFixture.data`), then the three story prompts. It honours `q`, `label` and `tag` (the list page sends them; the manifest ignored them). | Content: "listed by GET /prompts after the fixture prompts". |
| P1-c | The run answer is chosen from the request body in `playground.ts`: `body.source.prompt.name` picks the story; for `extract-invoice` only, `body.model` picks the column (starts with `claude` gives the right total, anything else the gemini one). No source, an unknown name or an unreadable body keeps today's "Thanks for your message…" answer. | A page opened with `?prompt=<name>&version=<n>` always carries the source; the model is the only thing that differs between the two extraction columns. |
| P1-d | `support-reply` keeps its fixtures (v2, labels `latest` and `staging`, `production` on v1). Only its run answer is new. | Content table row 1: "fixture exists, v2". |
| P1-e | A story run's `start.trace_id` is the story's hero trace id (support-reply `a0000000000000000000000000000003`, write-patch `a0000000000000000000000000000004`, voice-reply `a0000000000000000000000000000005`, extract-invoice `a0000000000000000000000000000006` for both columns); a run that names no story keeps the `TRACE_ID` constant. | So "Open trace" opens the hero trace in the large mock. The ids are literals in `playground.ts`, which has no dataset; a test checks them against `createDataset`. |

**Ids and dates**

Prompt ids (`…5707NN`, N = story number; N=1 is the fixture prompt `…1001`, unused here):
- write-patch `01900000-0000-7000-8000-000000570702`
- voice-reply `01900000-0000-7000-8000-000000570703`
- extract-invoice `01900000-0000-7000-8000-000000570704`

Dates are fixed, before the screenshots' clock (`2026-01-01T11:00:00Z`), format `YYYY-MM-DDTHH:MM:SS.000000000Z` like the fixtures. Check afterwards: `rtk proxy grep -rn "000000570[4-9]" fixtures web/src` shows only this module and its tests for `570702`–`570704`.

**Edit:**
- `web/src/mocks/stories-prompts.ts` (new)
- `web/src/mocks/stories-prompts.test.ts` (new)
- `web/src/mocks/playground.ts`
- `web/src/mocks/playground.test.ts` (new)
- `web/src/mocks/fallbacks.ts` (only: add `export` to `function metricsOf`)

No other file. In particular `fixtures/api/` is not edited, `web/src/mocks/node.ts` is not changed (tests call `server.use`), and `web/src/mocks/browser.ts` is P5's: the handlers are registered there, so a run in the dev server shows them only after P5.

**Contract: `web/src/mocks/stories-prompts.ts`** (add exactly; Biome may reformat; every string is product copy, keep every character):

```ts
import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import promptsFixture from "../../../fixtures/api/responses/prompts.json";
import { metricsOf } from "./fallbacks";

/**
 * The prompts of the homepage stories (design §12, M14), answered in the browser only. The fixture
 * prompts keep their own answers: these handlers return nothing for any other name, so the fallbacks
 * and the contract fixtures answer them. Never installed in tests; a test calls `server.use`.
 */

const PROJECT = "/api/v1/projects/:project";

type Template = { role: "system" | "user"; content: string }[];

/** One version of a story prompt, oldest first in `STORY_PROMPTS`. */
type StoryVersion = {
  template: Template;
  variables: string[];
  model: string;
  temperature: number;
  maxTokens: number;
  commit: string;
  at: string;
};

type StoryPrompt = {
  id: string;
  name: string;
  description: string;
  tags: string[];
  versions: StoryVersion[];
};

export const STORY_PROMPTS: StoryPrompt[] = [
  {
    id: "01900000-0000-7000-8000-000000570702",
    name: "write-patch",
    description: "Turn an issue and a file into a unified diff",
    tags: ["coding"],
    versions: [
      {
        template: [
          { role: "system", content: "You are a coding agent. Write a unified diff." },
          { role: "user", content: "Issue: {{ issue }}\n\n{{ code }}" },
        ],
        variables: ["issue", "code"],
        model: "claude-haiku-4-5",
        temperature: 0.2,
        maxTokens: 1024,
        commit: "First version",
        at: "2025-12-12T10:15:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "You are a coding agent. Write a unified diff that fixes the issue.",
          },
          { role: "user", content: "Issue: {{ issue }}\n\n{{ code }}" },
        ],
        variables: ["issue", "code"],
        model: "claude-sonnet-4-5",
        temperature: 0.2,
        maxTokens: 1024,
        commit: "Move to Sonnet and ask for a diff that fixes the issue",
        at: "2025-12-19T14:40:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "You are a coding agent. Write a unified diff that fixes the issue, and nothing else.",
          },
          { role: "user", content: "Issue: {{ issue }}\n\nFile {{ path }}:\n{{ code }}" },
        ],
        variables: ["issue", "path", "code"],
        model: "claude-sonnet-4-5",
        temperature: 0,
        maxTokens: 1024,
        commit: "Name the file and ask for the diff only",
        at: "2025-12-29T09:05:00.000000000Z",
      },
    ],
  },
  {
    id: "01900000-0000-7000-8000-000000570703",
    name: "voice-reply",
    description: "What the receptionist says back on the phone",
    tags: ["voice"],
    versions: [
      {
        template: [
          { role: "system", content: "You are a receptionist. Answer the caller." },
          { role: "user", content: "{{ caller }}" },
        ],
        variables: ["caller"],
        model: "gpt-4.1-mini",
        temperature: 0.5,
        maxTokens: 256,
        commit: "First version",
        at: "2025-12-10T16:20:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "You are a receptionist on the phone. Answer in two sentences.",
          },
          { role: "user", content: "{{ caller }}" },
        ],
        variables: ["caller"],
        model: "gpt-4.1-mini",
        temperature: 0.4,
        maxTokens: 256,
        commit: "Keep replies short for speech",
        at: "2025-12-16T11:00:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "You are a receptionist on the phone. Answer in two sentences.",
          },
          { role: "user", content: "{{ caller }}\n\nFree slots: {{ slots }}" },
        ],
        variables: ["caller", "slots"],
        model: "gpt-4.1-mini",
        temperature: 0.4,
        maxTokens: 256,
        commit: "Offer the free slots",
        at: "2025-12-22T15:30:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "You are a receptionist on the phone. Answer in one or two short sentences.",
          },
          { role: "user", content: "{{ caller }}\n\nFree slots: {{ slots }}" },
        ],
        variables: ["caller", "slots"],
        model: "gpt-4.1-mini",
        temperature: 0.3,
        maxTokens: 120,
        commit: "One or two sentences, so the reply starts sooner",
        at: "2025-12-30T10:10:00.000000000Z",
      },
    ],
  },
  {
    id: "01900000-0000-7000-8000-000000570704",
    name: "extract-invoice",
    description: "Read an invoice's fields from its text",
    tags: ["billing"],
    versions: [
      {
        template: [
          { role: "system", content: "Extract the invoice as JSON." },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gpt-4o-mini",
        temperature: 0,
        maxTokens: 512,
        commit: "First version",
        at: "2025-11-20T09:00:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "Extract the invoice as JSON: vendor, invoice_number, date, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gpt-4o-mini",
        temperature: 0,
        maxTokens: 512,
        commit: "Name the fields",
        at: "2025-11-27T13:20:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gemini-2.5-flash",
        temperature: 0.2,
        maxTokens: 512,
        commit: "Add the currency and the line items",
        at: "2025-12-04T10:45:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gemini-2.5-flash",
        temperature: 0,
        maxTokens: 512,
        commit: "Lower the temperature",
        at: "2025-12-15T08:30:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gemini-2.5-flash",
        temperature: 0,
        maxTokens: 600,
        commit: "Cap the output at 600 tokens",
        at: "2025-12-29T15:50:00.000000000Z",
      },
    ],
  },
];
```

Each prompt's shown version is its latest (write-patch 3, voice-reply 4, extract-invoice 5), so the prompt page opens on it with labels `latest` and `production`.

**Steps:**

1. Create `stories-prompts.ts` with the contract above, then these functions in the same file (bodies are short; write them as described):
   - `storyPrompt(name: unknown): StoryPrompt | undefined`: `STORY_PROMPTS.find((prompt) => prompt.name === name)`; `undefined` when `name` is not a string.
   - `promptOf(story: StoryPrompt): Schema<"Prompt">`: `{ id, name, type: "chat", description, tags, latest_version: story.versions.length, labels: [{ label: "production", version: story.versions.length, updated_at: <last version's at> }], created_at: <first version's at>, updated_at: <last version's at> }`.
   - `versionOf(story, version: number): Schema<"PromptVersion">`: for `version` in `1..story.versions.length`: `{ name: story.name, type: "chat", version, template: v.template, config: { model: v.model, params: { temperature: v.temperature, max_tokens: v.maxTokens } }, labels: version === latest ? ["latest", "production"] : [], variables: v.variables, placeholders: [], commit_message: v.commit, created_at: v.at }`, where `v = story.versions[version - 1]`. The caller has checked the range (steps below), so index access is safe; keep the `noUncheckedIndexedAccess` check by returning `undefined` when `v` is undefined and have callers fall through.
   - `versionsOf(story)`: every version, newest first.
   - `labelEventsOf(story)`: newest first, one event per version: `{ label: "production", to_version: n, at: v.at }` plus `from_version: n - 1` when `n > 1` (omit the key for version 1).
2. Export `storyPromptHandlers: HttpHandler[]`, in this order (a more specific route before a less specific one, as `fallbackHandlers` does):
   1. `GET ${PROJECT}/prompts/:prompt/metrics`: `storyPrompt(params.prompt)` undefined returns `undefined`; else read `from`/`to` from the URL like `fallbacks.ts` does (`to` default `new Date().toISOString()`, `from` default `to`) and answer `HttpResponse.json(metricsOf(<versions newest first as numbers>, from, to))`. This needs `export` on `metricsOf` in `fallbacks.ts` (its signature `(versions: number[], from: string, to: string)` is unchanged).
   2. `GET ${PROJECT}/prompts/:prompt/versions/:version`: story missing, or `Number(params.version)` not an integer in `1..latest`, returns `undefined`; else `HttpResponse.json(versionOf(...))`.
   3. `GET ${PROJECT}/prompts/:prompt/versions`: `{ data: versionsOf(story), next_cursor: null }`.
   4. `GET ${PROJECT}/prompts/:prompt/label-events`: `{ data: labelEventsOf(story), next_cursor: null }`.
   5. `GET ${PROJECT}/prompts/:prompt/resolve`: `label` query default `"production"`; `production` and `latest` both resolve to the latest version; any other label returns `undefined` (the manifest then answers 404).
   6. `GET ${PROJECT}/prompts/:prompt`: `HttpResponse.json(promptOf(story))`.
   7. `GET ${PROJECT}/prompts`: the page is `[...promptsFixture.data, ...STORY_PROMPTS.map(promptOf)]` filtered by the query: `q` (name contains it, ignoring case), `label` (some `labels[].label` equals it), `tag` (some tag equals it); answer `{ data, next_cursor: null }`. `promptsFixture.data` entries are typed from the JSON; cast the array to `Schema<"Prompt">[]` once, with a comment that the fixture is checked against the schema by the contract tests.
3. In `fallbacks.ts`: change `function metricsOf(` to `export function metricsOf(`. Nothing else.
4. In `playground.ts`, replace the constants `DELTAS` and the fixed numbers by an answer table. Contract (add exactly; keep every string):

   ```ts
   /** What a run answers: the text, streamed in three deltas, and the numbers of its `done` frame. */
   type Answer = {
     /** The trace the run's "Open trace" link opens. */
     trace_id: string;
     text: string;
     input: number;
     output: number;
     cost_usd: number;
     latency_ms: number;
     ttft_ms: number;
   };

   /** The generated dataset's hero traces (`SHOWCASES` in `dataset.ts`): `a` and the trace index in 31 hex digits. */
   const SUPPORT_TRACE_ID = "a0000000000000000000000000000003";
   const PATCH_TRACE_ID = "a0000000000000000000000000000004";
   const VOICE_TRACE_ID = "a0000000000000000000000000000005";
   const INVOICE_TRACE_ID = "a0000000000000000000000000000006";

   /** Any run that names no story: today's answer, and the file's existing `TRACE_ID`. */
   const GENERIC: Answer = {
     trace_id: TRACE_ID,
     text: "Thanks for your message. A support agent will reply within one business day.",
     input: 42,
     output: 18,
     cost_usd: 0.0004,
     latency_ms: 900,
     ttft_ms: 300,
   };

   const SUPPORT: Answer = {
     trace_id: SUPPORT_TRACE_ID,
     text: "Export from the Traces page, or call the export endpoint for large ranges.\n\n- Filter the traces you need\n- Choose Export and then CSV\n- Use the API for ranges over 100,000 traces",
     input: 88,
     output: 44,
     cost_usd: 0.00066,
     latency_ms: 1150,
     ttft_ms: 360,
   };

   const PATCH: Answer = {
     trace_id: PATCH_TRACE_ID,
     text: "```diff\n-    return value.replace('\"', '\"\"')\n+    escaped = value.replace('\"', '\"\"')\n+    return f'\"{escaped}\"' if needs_quotes(value) else escaped\n```",
     input: 412,
     output: 58,
     cost_usd: 0.0021,
     latency_ms: 2100,
     ttft_ms: 640,
   };

   const VOICE: Answer = {
     trace_id: VOICE_TRACE_ID,
     text: "Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you?",
     input: 74,
     output: 24,
     cost_usd: 0.00007,
     latency_ms: 540,
     ttft_ms: 190,
   };

   const INVOICE_FIELDS = {
     vendor: "Northwind Supplies",
     invoice_number: "INV-2026-0412",
     date: "2026-11-18",
     currency: "EUR",
     line_items: 3,
   };

   /** The first model's column: it reads the total as 1248, as the hero trace's first extraction did. */
   const INVOICE_WRONG: Answer = {
     trace_id: INVOICE_TRACE_ID,
     text: JSON.stringify({ ...INVOICE_FIELDS, total: 1248 }, null, 2),
     input: 1480,
     output: 160,
     cost_usd: 0.00084,
     latency_ms: 1450,
     ttft_ms: 520,
   };

   /** The second model's column: it reads the total right, 1284, as the hero trace's retry did. */
   const INVOICE_RIGHT: Answer = {
     trace_id: INVOICE_TRACE_ID,
     text: JSON.stringify({ ...INVOICE_FIELDS, total: 1284 }, null, 2),
     input: 1530,
     output: 172,
     cost_usd: 0.0072,
     latency_ms: 3200,
     ttft_ms: 880,
   };

   const ANSWERS = new Map<string, Answer>([
     ["support-reply", SUPPORT],
     ["write-patch", PATCH],
     ["voice-reply", VOICE],
   ]);
   ```

   (Cost arithmetic, for the record: support gpt-4o 88 in and 44 out at 2.5 and 10 USD per million; write-patch claude-sonnet-4-5 412 and 58 at 3 and 15; voice gpt-4.1-mini 74 and 24 at 0.40 and 1.60; extraction gemini-2.5-flash 1480 and 160 at 0.30 and 2.50; claude-sonnet-4-5 1530 and 172 at 3 and 15. Rounded to two significant digits.)

5. In `playground.ts`, add `answerOf` and use it:

   ```ts
   /** The shape of the run body this mock reads; any other field is ignored. */
   type RunBody = { model?: unknown; source?: { prompt?: { name?: unknown } } };

   /**
    * The answer for a run. The prompt the draft came from (`source.prompt.name`) picks the story; for
    * `extract-invoice` the column's model picks between its two answers (a model whose id starts with
    * `claude` reads the total right). Anything else, including a body that is not JSON, gets GENERIC.
    */
   function answerOf(body: unknown): Answer {
     const name = (body as RunBody | null)?.source?.prompt?.name;
     if (typeof name !== "string") {
       return GENERIC;
     }
     if (name === "extract-invoice") {
       const model = (body as RunBody).model;
       return typeof model === "string" && model.startsWith("claude") ? INVOICE_RIGHT : INVOICE_WRONG;
     }
     return ANSWERS.get(name) ?? GENERIC;
   }
   ```

   Then: `deltasOf(text: string): string[]` splits `text.match(/\S+\s*/g) ?? [text]` into three consecutive groups of `Math.ceil(words.length / 3)` words each and joins each group with `""` (so the concatenation equals `text` exactly; a text of fewer than three words gives fewer deltas, that is fine). `frames(answer: Answer)` replaces `frames()`: `start` with `answer.trace_id`, one `text` frame per delta (`wait: STEP_MS`), then `done` with `finish_reason: "stop"`, `usage: { input, output, cache_read: 0, cache_write: 0, reasoning: 0 }`, `cost_usd`, `latency_ms`, `ttft_ms`, `output: { role: "assistant", parts: [{ type: "text", content: answer.text }] }`. The resolver becomes `async ({ request }) => { … }`: read `await request.json()` inside a `try`; on a parse error use `undefined` as body; call `frames(answerOf(body))`. Keep the stream, the timers and the cancel code as they are. `answerOf` is not exported: the tests go through the handler.

   Do NOT keep the old `DELTAS` constant.
6. The two story columns of extract-invoice show different numbers because `INVOICE_WRONG` and `INVOICE_RIGHT` differ in tokens, cost, latency and ttft (table below).

**Answers per story (the content, for checking):**

| Story | Prompt, model | Answer | Input / output tokens | Cost USD | Latency ms | TTFT ms |
|---|---|---|---|---|---|---|
| 1 | support-reply v2, gpt-4o | the export answer (SUPPORT) | 88 / 44 | 0.00066 | 1150 | 360 |
| 2 | write-patch v3, claude-sonnet-4-5 | the diff (PATCH) | 412 / 58 | 0.0021 | 2100 | 640 |
| 3 | voice-reply v4, gpt-4.1-mini | the 2 pm / 4:30 pm reply (VOICE) | 74 / 24 | 0.00007 | 540 | 190 |
| 4, column 1 | extract-invoice v5, gemini-2.5-flash | JSON, `"total": 1248` (wrong) | 1480 / 160 | 0.00084 | 1450 | 520 |
| 4, column 2 | extract-invoice v5, claude-sonnet-4-5 | JSON, `"total": 1284` (right) | 1530 / 172 | 0.0072 | 3200 | 880 |

**Tests**

`web/src/mocks/stories-prompts.test.ts` (same setup as `fallbacks.test.ts`: `beforeEach(() => server.use(...storyPromptHandlers))`, a `get(path)` helper on `/api/v1/projects/acme%2Fsupport-bot`):
- `"lists the fixture prompts, then the three story prompts"`: names in order `summarize`, `support-reply`, `write-patch`, `voice-reply`, `extract-invoice`; `next_cursor` is null.
- `"filters the list by q, label and tag"`: `q=INVOICE` gives `extract-invoice` only; `tag=voice` gives `voice-reply`; `label=production` gives every prompt that has a `production` label (all five); `q=zzz` gives an empty list.
- `"gives each story prompt its shown version as the latest, labelled latest and production"`: `GET /prompts/<name>` has `latest_version` 3, 4, 5 and `type: "chat"`; `GET /prompts/<name>/versions` has 3, 4, 5 entries, newest first; the first entry's `labels` is `["latest", "production"]` and the others' `labels` are `[]`.
- `"serves the shown version's template, variables and model"`: `GET /prompts/write-patch/versions/3` has `variables` `["issue", "path", "code"]`, config model `claude-sonnet-4-5`, `commit_message` "Name the file and ask for the diff only", and the user template `"Issue: {{ issue }}\n\nFile {{ path }}:\n{{ code }}"`; `voice-reply` v4 variables `["caller", "slots"]`, model `gpt-4.1-mini`; `extract-invoice` v5 variables `["ocr_text"]`, model `gemini-2.5-flash`, `max_tokens` 600.
- `"resolves production and latest to the latest version, and leaves other labels to the manifest"`: `/prompts/voice-reply/resolve` and `?label=latest` have `version` 4; `?label=staging` is 404.
- `"records one label event per version, newest first"`: `label-events` of `extract-invoice` has 5 entries, `to_version` 5 down to 1, the last without `from_version`, the first with `from_version` 4.
- `"answers prompt metrics for any window, newest version first"`: `/prompts/write-patch/metrics?from=…&to=…` returns the window and versions `[3, 2, 1]`.
- `"leaves an unknown prompt, a version out of range and the fixture prompts to other handlers"`: `/prompts/nope` 404; `/prompts/write-patch/versions/4` 404; `/prompts/write-patch/versions/0` 404; `/prompts/support-reply` has `name` `support-reply` (the manifest answers it).
- `"gives every story prompt an id of its own"`: ids are unique and equal `01900000-0000-7000-8000-000000570702`, `…703`, `…704`; every version's `created_at` is before `2026-01-01T11:00:00Z` and the versions' dates increase.

`web/src/mocks/playground.test.ts` (setup: `server.use(...playgroundHandlers)`; helper `run(body)` that `POST`s `${location.origin}/api/v1/projects/acme%2Fsupport-bot/playground/runs` with `JSON.stringify(body)` and returns the frames parsed from `await response.text()`: split on `"\n\n"`, each block `event: <name>\ndata: <json>`; run the cases of one test with `Promise.all` since a run streams for about 750 ms; real timers):
- `"keeps today's answer for a run with no source, an unknown prompt or an unreadable body"`: bodies `{}`, `{ source: { prompt: { name: "summarize", version: 1 } } }`, and a raw non-JSON body each stream the text "Thanks for your message. A support agent will reply within one business day." and `done` with usage input 42 and output 18, cost 0.0004, latency 900, ttft 300; `start.trace_id` is `0190a1b2c3d4e5f60718293a4b5c6d7e`.
- `"answers each story prompt with its text and numbers"`: for `support-reply`, `write-patch` and `voice-reply` (body `{ model: "<any>", source: { prompt: { name, version } } }`) the concatenated `text` deltas equal the table's text exactly, the `done` frame carries the table's usage, cost, latency and ttft, and `done.output.parts[0].content` equals the same text.
- `"picks the extraction column by model"`: with source `extract-invoice` v5, `model: "gemini-2.5-flash"` and `model: "gpt-5-mini"` both answer JSON whose parsed `total` is 1248; `model: "claude-sonnet-4-5"` answers `total` 1284; the three `done` frames carry the table's numbers (column 2: 1530/172, 0.0072, 3200, 880).
- `"streams start, the text deltas, then done"`: the frame names are `start`, then one or more `text` (at most three), then `done`; a run with no story keeps `start.trace_id` `0190a1b2c3d4e5f60718293a4b5c6d7e`.
- `"opens the story's hero trace"`: with `const dataset = createDataset({ seed: 1, now: Date.parse("2026-06-01T00:00:00.000Z"), traces: 10 })` (import `createDataset` from `@/mocks/generated/dataset`), the `start.trace_id` of a run for `support-reply`, `write-patch`, `voice-reply` and `extract-invoice` (both models) equals `dataset.traces[3]`, `[4]`, `[5]` and `[6]` `.trace_id`.
- `"gives both extraction columns the same fields but the total"`: the two parsed answers are equal once `total` is removed, and both have `line_items` 3 (a number, as in the hero trace's payload).

`fallbacks.test.ts` is not edited; it must still pass (`summarize`, `support-reply` metrics, issue sources).

**Done when:** from the worktree root, after the setup line in the header:
- `pnpm --dir web exec vitest run src/mocks/stories-prompts.test.ts src/mocks/playground.test.ts src/mocks/fallbacks.test.ts --maxWorkers=8` passes;
- `pnpm --dir web run typecheck` passes;
- `./node_modules/.bin/biome check --write web/src/mocks/stories-prompts.ts web/src/mocks/stories-prompts.test.ts web/src/mocks/playground.ts web/src/mocks/playground.test.ts web/src/mocks/fallbacks.ts`, then the same command without `--write`, passes. Don't change any text by hand to please Biome.
- `rtk proxy grep -rn "000000570[4-9]" fixtures web/src` lists only `stories-prompts.ts`/its test (the `570702`–`570704` ids) and, if earlier phases already landed, their own ids; nothing in `fixtures/`.
- The by-hand check (the prompts list, the playground runs) is P5's: the handlers are registered there.

**Commit:** `Add the story prompts and playground answers to the mock`

---

### P2 Story dashboards

**Goal:** Four dashboards, one per homepage story, are defined in a browser-mock module (P5 seeds the store with it), and the large mock answers every widget with data.

**Verified against the code (what the steps rely on):**
- A `Definition` is only `{ widgets }` (`crates/server/openapi.json`, `additionalProperties: false`). There is **no dashboard-level filter** in a stored dashboard: the page's filter is the `filter` URL param. So the story's tag filter goes in **each widget's `query.filter`** (`WidgetQueryBody.filter`, which the large mock's `widget-query.ts` compiles against the source's columns).
- `tags @> ['x']` is evaluated by the large mock (`contains` on the list column `tags`, traces and spans sources). The **scores source has no `tags` column** (`COLUMNS.scores` = name, value_num, data_type, target) and a filter on it answers 400 `invalid_filter`. Spec 05 gives scores the same fields, so this is not only a mock limit.
- Number charts need exactly one measure and no breakdown; `bar_h` needs a breakdown (`validate.ts`). `rate` takes no `field` and needs a `where`; `p95` and `avg` need a numeric field of the source (`duration_ns`, `cost_total`, `value_num`). `count` takes no field.
- `where` and `filter` accept `status = 'error'`, `error_count > 0`, `duration_ns > 3000000000` (numeric compare on a number column), and `name = 'correctness'` (scores).
- The page sends, per widget: series (bucket) for line/bar; total for bar_h; both for number (total with `compare: true`, series for the sparkline). The large mock answers all of these.
- Dataset facts (`generated/dataset.ts`): generated traces carry 0-2 random tags of billing, priority, voice, email, retry (about 20% of 5,000 traces each), 8% are `status = 'error'`, `duration_ns` is uniform 0.2 s to 20.2 s, except that P0 makes voice-tagged traces last 0.4 s to 4 s (so about 28% of voice calls last over 3 s once P0 is merged; the tests here read the dataset, so they hold with or without P0), one model each of six, ages mostly within 30 days. Even-index traces carry a `correctness` score (0.1 to 1.0). Story hero traces (indices 3-6) carry tags email, retry, voice, billing.
- Seeding: `seedDashboards()` must keep returning the one fixture dashboard (`dashboards.test.ts` asserts a one-item list; `api/dashboards.test.tsx`, `dashboard-dialogs.test.tsx`, `dashboard-edit.test.tsx`, `manifest.test.ts` use it). Only `browser.ts` builds the dev/demo worker, so it is the only place to add the story dashboards: P5 does it, with `[...seedDashboards(), ...storyDashboards()]`.

**Edit:**
- `web/src/mocks/story-dashboards.ts` (new)
- `web/src/mocks/story-dashboards.test.ts` (new)

Nothing else may change (`dashboards.ts`, `generated/*`, fixtures stay as they are). `web/src/mocks/browser.ts` is P5's: it seeds the store with these dashboards.

**URLs** (large mock, project `acme/support-bot`; add `?range=30d` only if a shot needs a fixed range; the default range works):
| Dashboard | URL |
|---|---|
| Answer quality | `/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570401` |
| Code agent health | `/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570402` |
| Voice latency | `/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570403` |
| Invoice extraction | `/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570404` |

**Steps:**

1. Create `web/src/mocks/story-dashboards.ts` with exactly this content:

```ts
import type { Dashboard, Widget, WidgetQuery } from "@/lib/dashboards/types";

/**
 * The homepage stories' dashboards (design §12, M14), one per story. Each widget carries its
 * story's tag in `query.filter`, since a stored dashboard has no filter of its own; the scores
 * widget cannot (scores have no tags). The widget titles are the ones the assistant's draft
 * cards show. The browser mock stores them next to the fixture dashboard (`browser.ts`); the
 * large mock answers every widget.
 */

/** A fixture user (`fixtures/api/static/dashboard.json`). */
const AUTHOR = "01900000-0000-7000-8000-000000000100";
const CREATED_AT = "2026-11-24T09:00:00.000000000Z";

export const STORY_DASHBOARD_IDS = {
  answerQuality: "01900000-0000-7000-8000-000000570401",
  codeAgentHealth: "01900000-0000-7000-8000-000000570402",
  voiceLatency: "01900000-0000-7000-8000-000000570403",
  invoiceExtraction: "01900000-0000-7000-8000-000000570404",
} as const;

/** The 2x2 grid of a story dashboard: widgets 0 and 1 on the first row, 2 and 3 on the second. */
const SLOTS = [
  { x: 0, y: 0, w: 30, h: 6 },
  { x: 30, y: 0, w: 30, h: 6 },
  { x: 0, y: 6, w: 30, h: 6 },
  { x: 30, y: 6, w: 30, h: 6 },
] as const;

function chart(
  slot: 0 | 1 | 2 | 3,
  id: string,
  title: string,
  type: "line" | "bar" | "bar_h" | "number",
  query: WidgetQuery,
): Widget {
  return { id, title, layout: { ...SLOTS[slot] }, kind: "chart", chart: { type }, query };
}

function dashboard(
  id: string,
  name: string,
  description: string,
  widgets: Widget[],
): Dashboard {
  return {
    id,
    name,
    description,
    definition: { widgets },
    version: 1,
    created_by: AUTHOR,
    updated_by: AUTHOR,
    created_at: CREATED_AT,
    updated_at: CREATED_AT,
  };
}

const EMAIL = "tags @> ['email']";
const RETRY = "tags @> ['retry']";
const VOICE = "tags @> ['voice']";
const BILLING = "tags @> ['billing']";

/** The story dashboards, in story order. Fresh objects on every call. */
export function storyDashboards(): Dashboard[] {
  return [
    dashboard(
      STORY_DASHBOARD_IDS.answerQuality,
      "Answer quality",
      "Support answers: quality, volume and cost.",
      [
        chart(0, "correctness-by-day", "Correctness by day", "line", {
          source: "scores",
          filter: "name = 'correctness'",
          measures: [{ agg: "avg", field: "value_num", label: "Correctness" }],
        }),
        chart(1, "answers-per-day", "Answers per day", "bar", {
          source: "traces",
          filter: EMAIL,
          measures: [{ agg: "count", label: "Answers" }],
        }),
        chart(2, "cost-per-answer", "Cost per answer", "number", {
          source: "traces",
          filter: EMAIL,
          measures: [{ agg: "avg", field: "cost_total", label: "Cost per answer" }],
        }),
        chart(3, "answers-by-model", "Answers by model", "bar_h", {
          source: "traces",
          filter: EMAIL,
          measures: [{ agg: "count", label: "Answers" }],
          breakdown: { field: "models", limit: 5 },
        }),
      ],
    ),
    dashboard(
      STORY_DASHBOARD_IDS.codeAgentHealth,
      "Code agent health",
      "Failed runs, cost and duration of the coding agent.",
      [
        chart(0, "failed-runs-per-day", "Failed runs per day", "bar", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "count", where: "status = 'error'", label: "Failed runs" }],
        }),
        chart(1, "error-rate", "Error rate", "number", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "rate", where: "error_count > 0", label: "Error rate" }],
        }),
        chart(2, "cost-per-run", "Cost per run", "line", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "avg", field: "cost_total", label: "Cost per run" }],
        }),
        chart(3, "p95-duration", "p95 duration", "number", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "p95", field: "duration_ns", label: "p95 duration" }],
        }),
      ],
    ),
    dashboard(
      STORY_DASHBOARD_IDS.voiceLatency,
      "Voice latency",
      "How long callers wait for a reply.",
      [
        chart(0, "p95-reply-time", "p95 reply time", "number", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "p95", field: "duration_ns", label: "p95 reply time" }],
        }),
        chart(1, "reply-time-by-day", "Reply time by day", "line", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "p95", field: "duration_ns", label: "p95 reply time" }],
        }),
        chart(2, "calls-over-3-s", "Calls over 3 s", "number", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "rate", where: "duration_ns > 3000000000", label: "Over 3 s" }],
        }),
        chart(3, "calls-per-day", "Calls per day", "bar", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "count", label: "Calls" }],
        }),
      ],
    ),
    dashboard(
      STORY_DASHBOARD_IDS.invoiceExtraction,
      "Invoice extraction",
      "Invoices read, schema-check failures and cost.",
      [
        chart(0, "invoices-per-day", "Invoices per day", "bar", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "count", label: "Invoices" }],
        }),
        chart(1, "schema-check-failures", "Schema check failures", "number", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "rate", where: "status = 'error'", label: "Failures" }],
        }),
        chart(2, "cost-per-invoice", "Cost per invoice", "line", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "avg", field: "cost_total", label: "Cost per invoice" }],
        }),
        chart(3, "invoices-by-model", "Invoices by model", "bar_h", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "count", label: "Invoices" }],
          breakdown: { field: "models", limit: 5 },
        }),
      ],
    ),
  ];
}
```

2. Create `web/src/mocks/story-dashboards.test.ts` (imports: `beforeEach, expect, it` from vitest; `createDataset`, `visibleTraces` from `@/mocks/generated/dataset`; `largeHandlers` from `@/mocks/generated/handlers`; `server` from `@/mocks/node`; `storyDashboards`, `STORY_DASHBOARD_IDS` from `@/mocks/story-dashboards`; `validateDashboard` from `@/lib/dashboards/validate`; `dashboardHandlers`, `seedDashboards` from `@/mocks/dashboards`; `Schema` from `@/api/types`). Constants: `NOW = Date.parse("2026-11-19T12:00:00.000Z")`, `DAY_MS = 86_400_000`, `PROJECT = \`${location.origin}/api/v1/projects/acme%2Fsupport-bot\``, range `from = NOW - 30 DAY`, `to = NOW + DAY`. Build `const dataset = createDataset({ seed: 1, now: NOW, traces: 5000 })` once at module level. Helper `ask(widget, shape)` posts `{ from, to, shape, bucket: "1d" (series only), query: widget.query, compare: true (number total only) }` to `${PROJECT}/dashboards/query` and returns `{ status, json }`. A `beforeEach` registers `server.use(...largeHandlers(dataset))`.
3. Run the tests below.

**Tests** (all in `story-dashboards.test.ts`):
- `the story dashboards have the planned ids, names and widget titles`: exact ids `…570401`-`…570404`; names "Answer quality", "Code agent health", "Voice latency", "Invoice extraction"; the 4 titles of each in order as in the fixed content; every id unique; every widget on the 2x2 grid (x 0 or 30, w 30).
- `every story dashboard passes the dashboard validator`: `validateDashboard({ name, description, definition })` returns `[]` for each; description at most 500 chars.
- `storyDashboards returns fresh objects`: mutating one result does not change the next call.
- `seedDashboards still returns only the fixture dashboard`: `seedDashboards()` has length 1.
- `the browser mock store lists the story dashboards after the fixture one`: `server.use(...dashboardHandlers([...seedDashboards(), ...storyDashboards()]))`; `GET /dashboards` returns 5 names sorted by name; `GET /dashboards/<id>` for each story returns its definition with 4 widgets.
- `the large mock answers every widget of every story dashboard with data` (loop over 16 widgets): shape is `series` for `line`/`bar`/`number`, `total` for `bar_h`/`number`; status 200; series: at least one series with at least one point whose `value` is a number; total: `rows` non-empty and `rows[0].values[0]` is a number (not null); `bar_h` rows at least 2 groups. Number widgets are asked both ways.
- `the tag filter reaches only tagged traces`: for the Voice "Calls per day" widget, the sum of points equals `visibleTraces(dataset.traces,false,false)` count in range with tag `voice`.
- `the rate widgets match the dataset`: "Error rate" (code agent) equals share of retry-tagged visible traces with `error_count > 0` (toBeCloseTo); "Calls over 3 s" equals the share of voice-tagged visible traces with `duration_ns > 3_000_000_000`.
- `the hero traces are counted`: each story's hero trace (dataset indices 3, 4, 5, 6) is among the traces its dashboard's traces filter keeps: `tracePredicate(filter)(dataset.traces[i])` is true for 3 (email), 4 (retry), 5 (voice), 6 (billing) (import `tracePredicate` from `@/mocks/generated/widget-query`).

**Done when:**
- `pnpm --dir web exec vitest run src/mocks/story-dashboards.test.ts src/mocks/dashboards.test.ts src/mocks/generated/widget-query.test.ts src/api/dashboards.test.tsx --maxWorkers=8` passes;
- `pnpm --dir web run typecheck` passes;
- `./node_modules/.bin/biome check --write web/src/mocks/story-dashboards.ts web/src/mocks/story-dashboards.test.ts`, then the same command without `--write`, passes;
- the by-hand check (the four URLs above show four widgets with data and no error card) is P5's: the dashboards are seeded there.

**Commit:** `Add the story dashboards to the browser mock`

**Deviations from the fixed content**
1. **"Correctness by day"**: scores source, `avg value_num`, `filter: "name = 'correctness'"`. It does not carry the `email` tag filter (scores have no tags; the mock and spec 05 refuse it). In the mock every trace with a score counts, not only email ones. The dashboard's other three widgets are email-scoped.
2. **Filter placement**: the content's "Filter" column is applied per widget (`query.filter`), since a stored dashboard has no filter field. The dashboard-level filter box on the page stays empty.
3. **"Failed runs per day"**: `count` with `where: "status = 'error'"` (a measure condition). The line-like gaps: days with no failure show 0 (count over all rows then filtered), not a gap.
4. **Voice latency numbers**: P0 makes generated voice traces last 0.4 to 4 s, so "Calls over 3 s" shows about 28% and "p95 reply time" about 3.8 s once P0 is merged. These are the data, not a mock gap.
5. **Cost values** are tiny (dollars per trace of about 0.001 to 0.005), shown by the page's cost formatter.

---

### P3 Story annotation queues

**Goal:** the large mock serves one annotation queue per story (name, instructions, counts, score fields) whose item is the story's hero trace, and the five new score definitions, so each story's review form opens on its own URL.

**Edit:**
- `web/src/mocks/generated/stories.ts` (new queue data, definitions, handlers; `storyHandlers` gains the queue routes);
- `web/src/mocks/generated/handlers.ts` (serve the new definitions after the generated ones; nothing else);
- `web/src/mocks/generated/stories.test.ts` (new tests);
- `web/src/mocks/generated/handlers.test.ts` (only the test `"defines the generated scores so lists can colour them"`, plus two new tests).

No other file. Do not edit `fixtures/api/static/annotation-queue*.json`: the Rust round-trip tests, the review route tests and the README pin them.

**Where the data lives (decision, with reason).** In `stories.ts`, not a new module and not `handlers.ts`: it already holds the M13 story tables keyed by hero index, it is already spread first into `largeHandlers`, and its handlers already fall through (`undefined`) for ids that are not a story's. `handlers.ts` cannot be imported by `stories.ts` (it imports `stories.ts`), so `stories.ts` names the three generated definitions it reuses (correctness, relevance and tone) by id constants, and a test checks those ids are in the served definitions.

**Verified facts about the code (do not re-derive):**
- `queueItemHandlers` (in `handlers.ts`) answers `GET/POST …/annotation-queues/:id…` for every id with the fixture queue (definitions = first two of `SCORE_DEFINITIONS`) and the fixture item (trace = `dataset.traces[0]`). It stays as it is. The story handlers come before it in `largeHandlers`, answer only story ids and return `undefined` for any other id, so `…/000000000a01` and `…/000000000a11` keep today's answers (the docs shots `annotation-queue-review` and `marketing-queue-review` depend on that).
- The review form shows the queue's `definition_ids` in order, taken from `GET …/score-definitions` (`liveScoreDefinitionsQuery`: live ones, limit 1000). An id missing from that list is silently dropped. So the new definitions must be in the served list.
- The review page calls: `GET …/annotation-queues/{id}`, `GET …/items/{item_id}`, `POST …/items/{item_id}/lease`, `POST …/annotation-queues/{id}/next`, `GET …/score-definitions`, the trace detail and the target's scores. The item must be `pending` and leased by the mock user `01900000-0000-7000-8000-000000000100` (as the fixture item is).
- Hero trace ids are `dataset.traces[index].trace_id` (`a` followed by the index in 31 hex digits): index 3 support, 4 coding, 5 voice, 6 extraction (the `hero.index` values of `STORY_ISSUES`).
- The queue list page and the "In n queues" chip both call `GET …/annotation-queues`; the chip adds `target_trace_id`. The queue page's items table calls `GET …/annotation-queues/{id}/items` with `status` optional.

**Steps:**

1. **Types and constants** in `stories.ts`, after `STORY_RULES`:
   ```ts
   const MEMBER_ID = "01900000-0000-7000-8000-000000000100";
   /** The generated definitions the queues reuse (`SCORE_DEFINITIONS` in handlers.ts): correctness and relevance for Support answers, tone for Call review. */
   const CORRECTNESS_ID = "01900000-0000-7000-8000-000000000611";
   const RELEVANCE_ID = "01900000-0000-7000-8000-000000000612";
   const TONE_ID = "01900000-0000-7000-8000-000000000615";
   const DEFINED_AT = "2026-01-01T09:00:00.000000000Z";
   const LEASE_MS = 5 * MINUTE_MS;

   type StoryQueue = {
     id: string;
     itemId: string;
     name: string;
     description: string;
     instructions: string;
     /** The review fields, in order. */
     definitionIds: readonly string[];
     counts: Schema<"QueueCounts">;
     /** Dataset index of the hero trace, the queue's one item. */
     heroIndex: number;
   };
   ```

2. **Definitions.** Export the five new definitions, in this order, exactly (there is no `…570604`: Call review reuses the generated `tone`, `…000615`):
   ```ts
   export const STORY_SCORE_DEFINITIONS: Schema<"ScoreDefinition">[] = [
     {
       id: "01900000-0000-7000-8000-000000570601",
       name: "tests_pass",
       data_type: "boolean",
       categories: [],
       description: "Whether the tests pass after the patch.",
       direction: "higher",
       created_at: DEFINED_AT,
       updated_at: DEFINED_AT,
     },
     {
       id: "01900000-0000-7000-8000-000000570602",
       name: "verdict",
       data_type: "categorical",
       categories: [{ label: "approve" }, { label: "request changes" }, { label: "reject" }],
       description: "Your review of the patch.",
       direction: "none",
       created_at: DEFINED_AT,
       updated_at: DEFINED_AT,
     },
     {
       id: "01900000-0000-7000-8000-000000570603",
       name: "resolved",
       data_type: "boolean",
       categories: [],
       description: "Whether the caller's request was handled.",
       direction: "higher",
       created_at: DEFINED_AT,
       updated_at: DEFINED_AT,
     },
     {
       id: "01900000-0000-7000-8000-000000570605",
       name: "schema_valid",
       data_type: "boolean",
       categories: [],
       description: "Whether the extracted invoice passes the schema check.",
       direction: "higher",
       created_at: DEFINED_AT,
       updated_at: DEFINED_AT,
     },
     {
       id: "01900000-0000-7000-8000-000000570606",
       name: "fields_correct",
       data_type: "categorical",
       categories: [{ label: "all" }, { label: "some" }, { label: "none" }],
       description: "How many fields match the invoice.",
       direction: "none",
       created_at: DEFINED_AT,
       updated_at: DEFINED_AT,
     },
   ];
   ```
   (The categorical `categories` carry only `label`; no `value`.)

3. **Queues.** Export, in this order, exactly:
   ```ts
   export const STORY_QUEUES: readonly StoryQueue[] = [
     {
       id: "01900000-0000-7000-8000-000000570501",
       itemId: "01900000-0000-7000-8000-000000570511",
       name: "Support answers",
       description: "Support answers that people review after the judges.",
       instructions:
         "Score each answer against the customer's question. Skip the ones without a question.",
       definitionIds: [CORRECTNESS_ID, RELEVANCE_ID],
       counts: { pending: 12, completed: 4, skipped: 1 },
       heroIndex: 3,
     },
     {
       id: "01900000-0000-7000-8000-000000570502",
       itemId: "01900000-0000-7000-8000-000000570512",
       name: "Patch review",
       description: "Patches from the coding agent, before they are merged.",
       instructions: "Check that the patch fixes the issue and that the tests pass.",
       definitionIds: [
         "01900000-0000-7000-8000-000000570601",
         "01900000-0000-7000-8000-000000570602",
       ],
       counts: { pending: 6, completed: 9, skipped: 0 },
       heroIndex: 4,
     },
     {
       id: "01900000-0000-7000-8000-000000570503",
       itemId: "01900000-0000-7000-8000-000000570513",
       name: "Call review",
       description: "Calls that the voice agent handled.",
       instructions:
         "Listen for whether the caller got what they asked for, and how the reply sounded.",
       definitionIds: ["01900000-0000-7000-8000-000000570603", TONE_ID],
       counts: { pending: 20, completed: 7, skipped: 2 },
       heroIndex: 5,
     },
     {
       id: "01900000-0000-7000-8000-000000570504",
       itemId: "01900000-0000-7000-8000-000000570514",
       name: "Invoice fields",
       description: "Invoices that the extraction pipeline read.",
       instructions: "Compare the extracted fields with the invoice.",
       definitionIds: [
         "01900000-0000-7000-8000-000000570605",
         "01900000-0000-7000-8000-000000570606",
       ],
       counts: { pending: 31, completed: 18, skipped: 3 },
       heroIndex: 6,
     },
   ];
   ```
   The progress line "N of M done" then reads 5 of 17, 9 of 15, 9 of 29 and 21 of 52 (completed + skipped, of the total). `description` is not in the content table; it only shows on the queue list. Use the texts above.

4. **Builders** (exported, for the tests):
   ```ts
   export function storyQueue(dataset: Dataset, queue: StoryQueue): Schema<"AnnotationQueue"> {
     return {
       id: queue.id,
       name: queue.name,
       description: queue.description,
       instructions: queue.instructions,
       target: "trace",
       definition_ids: [...queue.definitionIds],
       version: 1,
       created_by: MEMBER_ID,
       created_at: ago(dataset, 7 * DAY_MS),
       updated_at: ago(dataset, HOUR_MS),
       counts: queue.counts,
     };
   }

   /** The queue's one item: the hero trace, pending and leased to the mock user. Undefined when the dataset has no such trace. */
   export function storyQueueItem(
     dataset: Dataset,
     queue: StoryQueue,
   ): Schema<"AnnotationQueueItem"> | undefined {
     const hero: Trace | undefined = dataset.traces[queue.heroIndex];
     if (hero === undefined) {
       return undefined;
     }
     return {
       id: queue.itemId,
       queue_id: queue.id,
       trace_id: hero.trace_id,
       target_time: hero.start_time,
       status: "pending",
       leased_by_user_id: MEMBER_ID,
       lease_expires_at: rfc3339(dataset.now + LEASE_MS),
       added_by: "user",
       added_by_id: MEMBER_ID,
       created_at: ago(dataset, 6 * HOUR_MS),
     };
   }
   ```
   If `"trace"` or `"user"` is not accepted by the schema types (`QueueTarget`, `QueueItemAddedBy`), typecheck fails: use the fixture's values (`"trace"`, `"user"`; they are in `fixtures/api/static/annotation-queue*.json`) and report it. `QueueCounts` must be the schema's name of the `counts` type (`components["schemas"]["QueueCounts"]`).

5. **Handlers.** Add to the array `storyHandlers` returns (after the alert routes). First add, next to the other fixture imports of `stories.ts`:
   ```ts
   import queuesFixture from "../../../../fixtures/api/static/annotation-queues.json";
   ```
   and inside `storyHandlers`:
   ```ts
   const queueById = (id: string) => STORY_QUEUES.find((queue) => queue.id === id);
   const QUEUE = `${PROJECT}/annotation-queues`;
   ```
   Routes (all answer `undefined`, to fall through, when the queue id is not a story's or the hero trace is missing):
   | Route | Answer |
   |---|---|
   | `GET ${QUEUE}` | If the URL has `target_session_id`: `undefined`. If it has `target_trace_id`: the story queues whose hero trace id equals it, as `{ data, next_cursor: null }`; `undefined` when none. Otherwise `{ data: [...STORY_QUEUES.map(storyQueue), ...queuesFixture.data], next_cursor: null }`. Ignore `limit` and the cursor, as the manifest does. |
   | `GET ${QUEUE}/:id` | `storyQueue` |
   | `GET ${QUEUE}/:id/items` | `{ data: [item], next_cursor: null }`; `{ data: [], next_cursor: null }` when the `status` query is present and differs from `"pending"` |
   | `GET ${QUEUE}/:id/items/:item_id` | `storyQueueItem` when `item_id` is that queue's `itemId`, else `undefined` |
   | `POST ${QUEUE}/:id/items/:item_id/lease` | same item, same condition |
   | `POST ${QUEUE}/:id/next` | the item |
   `QUEUE` as a template literal ending `…/annotation-queues` does not match `…/annotation-queues/:id`; msw matches whole paths, so the six routes do not collide. Read ids with the existing `idParam`. Put a small local helper `const heroItem = (queue: StoryQueue) => storyQueueItem(dataset, queue)` to avoid repeating.

6. **`handlers.ts`.** Import `STORY_SCORE_DEFINITIONS` with `storyHandlers` (`import { STORY_SCORE_DEFINITIONS, storyHandlers } from "@/mocks/generated/stories";`) and change the `score-definitions` route to `HttpResponse.json({ data: [...SCORE_DEFINITIONS, ...STORY_SCORE_DEFINITIONS], next_cursor: null })`. Do not touch `SCORE_DEFINITIONS` or `queueItemHandlers`.

7. **`tone`.** Call review reuses the generated `tone` (`…000615`: neutral, friendly, formal, curt); the content's earlier warm/neutral/curt labels are not used. Do not add a second `tone` definition.

**Tests** (`stories.test.ts`; `NOW`, `PROJECT` and `createDataset({ seed: 1, now: NOW, traces: 2000 })` as in the file; import `STORY_QUEUES`, `STORY_SCORE_DEFINITIONS`, `storyQueue`, `storyQueueItem`):
- `"points every story queue's item at the story's hero trace"`: for each queue, `storyQueueItem(...)?.trace_id` equals `dataset.traces[heroIndex].trace_id`, its `queue_id` is the queue's, `status` is `"pending"`, and `heroIndex` equals the `hero.index` of the `STORY_ISSUES` entry at the same position.
- `"gives every story queue its own score fields"`: the four queues' `definition_ids` resolve, against `[...STORY_SCORE_DEFINITIONS, <a local map of the three generated ids: `…000611` correctness, `…000612` relevance, `…000615` tone>]`, to these names in order: `["correctness","relevance"]`, `["tests_pass","verdict"]`, `["resolved","tone"]`, `["schema_valid","fields_correct"]`; the five new definitions have ids `…570601`, `…570602`, `…570603`, `…570605`, `…570606` in that order (no `…570604`), and every categorical one has the labels listed in step 2.
- `"serves the story queues, their items and the lease, and lets other ids fall through"` (`server.use(...storyHandlers(dataset))`):
  - `GET …/annotation-queues` starts with the four story queue ids, then `01900000-0000-7000-8000-000000000a01`;
  - `GET …/annotation-queues?target_trace_id=<hero 4 id>` returns only `…570502`;
  - `GET …/annotation-queues/01900000-0000-7000-8000-000000570503` has the Call review name, instructions and counts `20/7/2`;
  - `GET …/annotation-queues/…570503/items/…570513` is the item with the voice hero's trace id, and `POST …/lease` on it answers the same item;
  - `GET …/annotation-queues/…570503/items?status=completed` is `{ data: [] }`;
  - `GET …/annotation-queues/01900000-0000-7000-8000-000000000a01` is not answered by the story handlers: it answers the fixture queue (name `Relevance review`).
- `"counts a story queue's progress as completed plus skipped of its total"`: for each queue, `completed + skipped` over `pending + completed + skipped` is `5/17`, `9/15`, `9/29`, `21/52`.
- In `handlers.test.ts`, change `"defines the generated scores so lists can colour them"` to expect the ten names in order, with these directions: the existing five unchanged, then `["tests_pass","higher"], ["verdict","none"], ["resolved","higher"], ["schema_valid","higher"], ["fields_correct","none"]`. This test may be edited because the served list now has the story definitions.
- Add `"serves the story queues' score definitions after the generated ones"`: `GET …/score-definitions` contains `…000611` (correctness), `…000612` (relevance) and `…000615` (tone) before the five story ids `…570601`, `…570602`, `…570603`, `…570605`, `…570606`, which are in that order.
- Add `"keeps the fixture queue's review as it was"`: with `largeHandlers(createDataset({ seed: 1, now: NOW, traces: 5 }))`, `GET …/annotation-queues/01900000-0000-7000-8000-000000000a01` has `definition_ids` equal to the ids `…611`, `…612`; `GET …/annotation-queues/01900000-0000-7000-8000-000000570501` has `…611`, `…612` too (the first story reuses them); `…570502` has `…570601`, `…570602`; `…570503` has `…570603` and `…000615`; and `GET …/annotation-queues/01900000-0000-7000-8000-000000570502/items/01900000-0000-7000-8000-000000570512` has the trace id of `dataset.traces[4]`, while the fixture item path `…/000000000a01/items/000000000a11` has `dataset.traces[0]`'s.
- If any other existing test fails because of the new handlers (route tests, datasets, writes), stop and report it with `QUESTION:`. Do not edit it.

**Review URLs** (M15 uses them; `org/project` are `acme/support-bot`):
1. `/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570501/review/01900000-0000-7000-8000-000000570511`
2. `/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570502/review/01900000-0000-7000-8000-000000570512`
3. `/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570503/review/01900000-0000-7000-8000-000000570513`
4. `/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570504/review/01900000-0000-7000-8000-000000570514`

**Done when** (from the worktree root, after `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`):
- `pnpm --dir web exec vitest run src/mocks 'src/routes/$org/$project/annotation-queues' src/components/annotation src/components/annotation-queues --maxWorkers=8` passes;
- `pnpm --dir web run typecheck` passes;
- `./node_modules/.bin/biome check --write web/src/mocks/generated/stories.ts web/src/mocks/generated/handlers.ts web/src/mocks/generated/stories.test.ts web/src/mocks/generated/handlers.test.ts`, then the same command without `--write`, passes. Don't change any text by hand to please Biome.
- By hand: none here; P5 opens a review page and M15 checks the pictures.

**Commit:** `Add each story's annotation queue to the large mock`

---

### P4 Story assistant conversations

**Goal:** the assistant mock answers each story's two questions from the request's message text, keeps both turns in one conversation whose stored messages equal what was streamed, and lists one stored two-turn conversation per story.

**Edit:** `web/src/mocks/assistant.ts`, `web/src/mocks/assistant.test.ts` (new). No other file.

**Cross-phase contract with P2.** Every widget in the four draft cards has exactly the title, id (the title lowercased, spaces to hyphens: `correctness-by-day`, `answers-per-day`, `cost-per-answer`, `answers-by-model`, `failed-runs-per-day`, `error-rate`, `cost-per-run`, `p95-duration`, `p95-reply-time`, `reply-time-by-day`, `calls-over-3-s`, `calls-per-day`, `invoices-per-day`, `schema-check-failures`, `cost-per-invoice`, `invoices-by-model`), chart type, layout and query filter of the same widget in P2's `story-dashboards.ts`; only the measure labels may differ. The voice card's third widget is "Calls over 3 s", a `rate` where `duration_ns > 3000000000`. P5 checks the titles.

**Contracts (facts checked in the code).**
- The turn request body is `{ message: string, context?: object }` (`AssistantTurnRequest`, `schema.gen.ts`). The panel sends `message` trimmed (`send` in `assistant-panel.tsx`). Today's handler ignores the body.
- After `done`, `assistant-panel.tsx` invalidates the conversation query: the stored messages (`bubblesOf`, `lib/assistant/transcript.ts`) replace the live bubbles. So the stored user message is what the screenshot shows as the question.
- The "View trace" link is `bubble.traceId`, which `bubblesOf` takes from the `turn_trace_id` of the bubble's last assistant message, and `applyEvent` from the `start` event's `trace_id`. Both are the story's hero trace id here.
- A step's label comes from `LABELS` in `tool-step.tsx`: `search_traces` is "Searched traces" and `draft_dashboard` is "Drafted a dashboard". Its summary is `N rows` when `rows` is set, else `<size_bytes> bytes`. The card of a `dashboard` draft shows `draft.name`, `draft.description` and the widgets' `title`s only (`DashboardCard` in `draft-card.tsx`); `parseDraft("dashboard", …)` (`lib/assistant/types.ts`) requires `name` (string), `definition.widgets` (array) and optional `description`.
- A stored chat is opened without typing through the chat menu: the header button has `aria-label="Chats: <title>"` (`"Chats: New chat"` before any is selected); its items show the conversation's title (`ConversationMenu`). (For M15's shots: click `button[aria-label^="Chats:"]`, then the item with the story's question as title.)
- Hero trace ids are `a` + the index in hex, padded to 32 characters: support 3, coding 4, voice 5, extraction 6 (`a0000000000000000000000000000003` … `…0006`). `createDataset({ seed: 1, now, traces: 50 })` has trace indexes 0 to 49, so every id below exists.
- The mock turn delay is `EVENT_DELAY_MS = 120` per event; tests need none.
- `resetAssistantMock()` has no caller today; the new test calls it in `beforeEach`.

**Decisions this phase makes (all follow the planner's content; none is a product decision).**
- The story's two turns happen in the one new conversation (`NEW_CONVERSATION_ID`, `…1a02`), as the typed flow does. The stored copies in the list use ids `01900000-0000-7000-8000-000000570801` (support), `…570802` (coding), `…570803` (voice), `…570804` (extraction), each with both turns.
- A turn is recorded only when it is run in the new conversation. A turn sent to any other conversation id streams its answer and stores nothing (before: it set the shared `turnRan` flag; that is dropped because it was wrong for ids other than `…1a02`).
- `POST …/conversations` empties the new conversation (before, a second "New chat" showed the first one's messages).
- Every turn run in the new conversation is appended, so a second default turn gives 12 messages (before: nothing changed). Stored `seq` and `created_at` are renumbered per position.
- Selection is `message.trim().toLowerCase()` compared with each story question lowercased. Anything else runs today's script, which does not change: its events, its stored messages, its title, its trace id and its stored context stay as they are.

**Steps.**

1. **Imports and state.** In `web/src/mocks/assistant.ts`:
   1. Add the import `import type { Widget, WidgetQuery } from "@/lib/dashboards/types";` (sorted by Biome).
   2. Add `export` to nothing else, except these two, which the test imports: `assistantHandlers` (already exported) and `resetAssistantMock` (already exported). The test also needs the story data, so export `STORIES` (step 3) and `scriptFor` (step 4).
   3. Replace the two state variables and `resetAssistantMock` with:
      ```ts
      /** One turn run in the new conversation: the script that answered and the context the request carried. */
      type Ran = { script: Script; context: Fields | undefined };

      /** Held in module state, as before; `resetAssistantMock()` clears it. */
      let created = false;
      let ran: Ran[] = [];

      export function resetAssistantMock(): void {
        created = false;
        ran = [];
      }
      ```
2. **Types and helpers.** Put this after `newConversation` (the `scriptedTitle`, `scriptedEvents`, `scriptedToolRows` and `scriptedMessages` stay where they are, unchanged):
   ```ts
   type Fields = Record<string, unknown>;
   type StreamEvent = { event: string; data: unknown };
   /** A stored message (D-AS-21 stored form) as the detail route returns it. */
   type StoredMessage = Fields & { seq: number };

   /** A script: the SSE events of a turn, and the messages the server stores for it, from `seq` 1. */
   type Script = {
     title: string;
     events: (turnId: number) => StreamEvent[];
     messages: (context: Fields | undefined) => StoredMessage[];
   };

   /** The time of message `seq`: `base` plus `seq` seconds, with nanosecond digits as the server writes them. */
   function stamp(base: string, seq: number): string {
     return new Date(Date.parse(base) + seq * 1000).toISOString().replace("Z", "000000Z");
   }

   /** `messages` moved to start at `firstSeq`, with their times. */
   function placed(messages: StoredMessage[], firstSeq: number, base: string): StoredMessage[] {
     return messages.map((message, index) => ({
       ...message,
       seq: firstSeq + index,
       created_at: stamp(base, firstSeq + index),
     }));
   }

   /** All the messages of `turns`, one after the other. */
   function storedMessages(turns: Ran[], base: string): StoredMessage[] {
     const stored: StoredMessage[] = [];
     for (const turn of turns) {
       stored.push(...placed(turn.script.messages(turn.context), stored.length + 1, base));
     }
     return stored;
   }

   function isFields(value: unknown): value is Fields {
     return typeof value === "object" && value !== null && !Array.isArray(value);
   }

   /** The question and page context of a turn request; an unreadable body is an empty question. */
   async function readTurnRequest(
     request: Request,
   ): Promise<{ message: string; context: Fields | undefined }> {
     try {
       const body: unknown = await request.json();
       if (!isFields(body)) {
         return { message: "", context: undefined };
       }
       return {
         message: typeof body.message === "string" ? body.message : "",
         context: isFields(body.context) ? body.context : undefined,
       };
     } catch {
       return { message: "", context: undefined };
     }
   }

   /** Today's script: one scripted turn, whatever the question was. */
   const DEFAULT_SCRIPT: Script = {
     title: scriptedTitle,
     events: () => scriptedEvents,
     messages: () => scriptedMessages(),
   };
   ```
   Do not edit `scriptedMessages()`: its `created_at` values (`2026-01-02T00:00:01…` to `…06`) equal `stamp(NEW_BASE, seq)` for seq 1 to 6, so `placed` returns them unchanged for a first default turn.
3. **The story scripts.** Add this after `DEFAULT_SCRIPT`:
   ```ts
   /** The call id of a story turn's one tool call. */
   const CALL_ID = "call_1";
   const NEW_BASE = "2026-01-02T00:00:00Z";

   /** What one story turn says and does. */
   type TurnSpec = {
     question: string;
     /** The text before the tool call, as two streamed deltas. */
     intro: [string, string];
     call: { name: string; arguments: Fields };
     /** The stored result of the call. */
     result: Fields;
     /** The rows of a search; absent for a draft. */
     rows?: number;
     draft?: { kind: "dashboard"; draft: Fields };
     answer: string;
   };

   const USAGE = { input: 1400, output: 160 };
   const COST_USD = 0.0024;

   function summaryOf(spec: TurnSpec) {
     const size_bytes = JSON.stringify(spec.result).length;
     return spec.rows === undefined ? { size_bytes } : { size_bytes, rows: spec.rows };
   }

   function storyEvents(spec: TurnSpec, traceId: string, turnId: number): StreamEvent[] {
     return [
       { event: "start", data: { turn_id: turnId, trace_id: traceId } },
       { event: "text", data: { delta: spec.intro[0] } },
       { event: "text", data: { delta: spec.intro[1] } },
       {
         event: "tool_call",
         data: { id: CALL_ID, name: spec.call.name, arguments: spec.call.arguments },
       },
       { event: "tool_result", data: { id: CALL_ID, ok: true, summary: summaryOf(spec) } },
       ...(spec.draft === undefined
         ? []
         : [
             {
               event: "draft",
               data: { id: `draft_${CALL_ID}`, kind: spec.draft.kind, draft: spec.draft.draft },
             },
           ]),
       { event: "text", data: { delta: spec.answer } },
       {
         event: "done",
         data: { finish_reason: "stop", usage: USAGE, cost_usd: COST_USD, steps: 1 },
       },
     ];
   }

   /** The four stored messages of a story turn: question, call, result (and draft), answer. */
   function storyMessages(
     spec: TurnSpec,
     traceId: string,
     context: Fields | undefined,
   ): StoredMessage[] {
     const reply = { status: "complete", turn_trace_id: traceId, created_at: "" };
     return [
       {
         seq: 1,
         role: "user",
         parts: [{ type: "text", content: spec.question }],
         ...(context === undefined ? {} : { context }),
         status: "complete",
         created_at: "",
       },
       {
         seq: 2,
         role: "assistant",
         parts: [
           { type: "text", content: spec.intro.join("") },
           {
             type: "tool_call",
             id: CALL_ID,
             name: spec.call.name,
             arguments: spec.call.arguments,
           },
         ],
         ...reply,
       },
       {
         seq: 3,
         role: "tool",
         parts: [
           {
             type: "tool_result",
             id: CALL_ID,
             name: spec.call.name,
             ok: true,
             result: spec.result,
             ...summaryOf(spec),
             stubbed: false,
           },
           ...(spec.draft === undefined
             ? []
             : [
                 {
                   type: "draft",
                   id: `draft_${CALL_ID}`,
                   kind: spec.draft.kind,
                   draft: spec.draft.draft,
                 },
               ]),
         ],
         ...reply,
       },
       { seq: 4, role: "assistant", parts: [{ type: "text", content: spec.answer }], ...reply },
     ];
   }

   function storyScript(spec: TurnSpec, traceId: string): Script {
     return {
       title: spec.question,
       events: (turnId) => storyEvents(spec, traceId, turnId),
       messages: (context) => storyMessages(spec, traceId, context),
     };
   }

   /** The id of the generated trace at `index`, as `dataset.ts` writes it. */
   function traceIdAt(index: number): string {
     return `a${index.toString(16).padStart(31, "0")}`;
   }

   /** A search result: one row per trace. */
   function searchResult(indexes: number[]): { result: Fields; rows: number } {
     return { result: { rows: indexes.map((index) => ({ trace_id: traceIdAt(index) })) }, rows: indexes.length };
   }

   const STORY_CONTEXT = { page: "traces", range: "24h" };

   /** One chart widget on the 2 x 2 grid of the 60-column dashboard: `slot` 0 to 3, left to right, top to bottom. */
   function widget(
     title: string,
     slot: number,
     type: "line" | "bar" | "bar_h" | "number",
     query: WidgetQuery,
   ): Widget {
     return {
       id: title.toLowerCase().replaceAll(" ", "-"),
       title,
       kind: "chart",
       layout: { x: (slot % 2) * 30, y: Math.floor(slot / 2) * 6, w: 30, h: 6 },
       chart: { type },
       query,
     };
   }

   /** A traces query for a story dashboard, filtered by the dashboard's tag. */
   function traces(filter: string, measure: WidgetQuery["measures"][number]): WidgetQuery {
     return { source: "traces", filter, measures: [measure] };
   }
   ```
   Then the four stories. Write them exactly (Biome may reformat; do not change a string):
   ```ts
   const SUPPORT_FILTER = "tags @> ['email']";
   const CODING_FILTER = "tags @> ['retry']";
   const VOICE_FILTER = "tags @> ['voice']";
   const BILLING_FILTER = "tags @> ['billing']";

   /** One story: its hero trace, its stored chat, and its two turns (an answer, then a dashboard draft). */
   export type Story = {
     name: "support" | "coding" | "voice" | "extraction";
     conversationId: string;
     /** When the stored chat started; each story is five minutes older than the one before. */
     base: string;
     heroTraceId: string;
     turns: [TurnSpec, TurnSpec];
   };

   function searchTurn(
     question: string,
     intro: [string, string],
     filter: string,
     from: string,
     found: { result: Fields; rows: number },
     answer: string,
   ): TurnSpec {
     return {
       question,
       intro,
       call: { name: "search_traces", arguments: { from, to: "2026-11-19T12:00:00Z", filter } },
       result: found.result,
       rows: found.rows,
       answer,
     };
   }

   function dashboardTurn(question: string, draft: Fields): TurnSpec {
     return {
       question,
       intro: ["I'll draft a dashboard ", "with four widgets. "],
       call: { name: "draft_dashboard", arguments: draft },
       result: { status: "drafted" },
       draft: { kind: "dashboard", draft },
       answer: "Here is the draft. Open it in the editor to review and save it.",
     };
   }

   export const STORIES: Story[] = [
     {
       name: "support",
       conversationId: "01900000-0000-7000-8000-000000570801",
       base: "2026-11-19T11:50:00Z",
       heroTraceId: traceIdAt(3),
       turns: [
         searchTurn(
           "Which answers scored low today?",
           ["I'll look at today's support answers ", "and their scores. "],
           SUPPORT_FILTER,
           "2026-11-19T00:00:00Z",
           searchResult([11, 18, 23, 31, 38, 44]),
           "Six answers scored under 0.5 today, most about refunds. The CSV export answer (support-agent) scored 0.9.",
         ),
         dashboardTurn("Build me a dashboard of answer quality.", {
           name: "Answer quality",
           description: "Support answers: quality, volume and cost.",
           definition: {
             widgets: [
               widget("Correctness by day", 0, "line", {
                 source: "scores",
                 filter: "name = 'correctness'",
                 measures: [{ agg: "avg", field: "value_num", label: "Correctness" }],
               }),
               widget("Answers per day", 1, "bar", traces(SUPPORT_FILTER, { agg: "count", label: "Answers" })),
               widget(
                 "Cost per answer",
                 2,
                 "number",
                 traces(SUPPORT_FILTER, { agg: "avg", field: "cost_total", label: "Cost" }),
               ),
               widget("Answers by model", 3, "bar_h", {
                 source: "traces",
                 filter: SUPPORT_FILTER,
                 measures: [{ agg: "count", label: "Answers" }],
                 breakdown: { field: "models", limit: 5 },
               }),
             ],
           },
         }),
       ],
     },
     {
       name: "coding",
       conversationId: "01900000-0000-7000-8000-000000570802",
       base: "2026-11-19T11:45:00Z",
       heroTraceId: traceIdAt(4),
       turns: [
         searchTurn(
           "Which traces failed in the last day?",
           ["I'll search the failed traces ", "from the last day. "],
           "status = 'error' and tags @> ['retry']",
           "2026-11-18T12:00:00Z",
           searchResult([4]),
           "One: coding-agent (a0000000…0004). Its first run_tests call failed with 1 failed, 47 passed, and the second run passed.",
         ),
         dashboardTurn("Build me a dashboard for code-agent: failed runs, cost per run and p95 duration.", {
           name: "Code agent health",
           description: "Failed runs, cost and duration of the coding agent.",
           definition: {
             widgets: [
               widget(
                 "Failed runs per day",
                 0,
                 "bar",
                 traces(CODING_FILTER, { agg: "count", where: "status = 'error'", label: "Failed runs" }),
               ),
               widget(
                 "Error rate",
                 1,
                 "number",
                 traces(CODING_FILTER, { agg: "rate", where: "error_count > 0", label: "Errors" }),
               ),
               widget(
                 "Cost per run",
                 2,
                 "line",
                 traces(CODING_FILTER, { agg: "avg", field: "cost_total", label: "Cost" }),
               ),
               widget(
                 "p95 duration",
                 3,
                 "number",
                 traces(CODING_FILTER, { agg: "p95", field: "duration_ns", label: "p95" }),
               ),
             ],
           },
         }),
       ],
     },
     {
       name: "voice",
       conversationId: "01900000-0000-7000-8000-000000570803",
       base: "2026-11-19T11:40:00Z",
       heroTraceId: traceIdAt(5),
       turns: [
         searchTurn(
           "Which calls were slow today?",
           ["I'll look for slow calls ", "from today. "],
           "tags @> ['voice'] and duration_ns > 3000000000",
           "2026-11-19T00:00:00Z",
           searchResult([5, 9, 13, 16, 21, 25, 28, 33, 37, 40, 43, 47]),
           "Twelve calls took over 3 s. In voice-agent (a0000000…0005), check_availability alone took 1.22 s.",
         ),
         dashboardTurn("Build me a dashboard of voice latency.", {
           name: "Voice latency",
           description: "How long callers wait for a reply.",
           definition: {
             widgets: [
               widget(
                 "p95 reply time",
                 0,
                 "number",
                 traces(VOICE_FILTER, { agg: "p95", field: "duration_ns", label: "p95" }),
               ),
               widget(
                 "Reply time by day",
                 1,
                 "line",
                 traces(VOICE_FILTER, { agg: "p95", field: "duration_ns", label: "p95" }),
               ),
               widget(
                 "Calls over 3 s",
                 2,
                 "number",
                 traces(VOICE_FILTER, {
                   agg: "rate",
                   where: "duration_ns > 3000000000",
                   label: "Over 3 s",
                 }),
               ),
               widget("Calls per day", 3, "bar", traces(VOICE_FILTER, { agg: "count", label: "Calls" })),
             ],
           },
         }),
       ],
     },
     {
       name: "extraction",
       conversationId: "01900000-0000-7000-8000-000000570804",
       base: "2026-11-19T11:35:00Z",
       heroTraceId: traceIdAt(6),
       turns: [
         searchTurn(
           "Which invoices needed a retry?",
           ["I'll look for invoices ", "that ran more than once. "],
           BILLING_FILTER,
           "2026-11-18T12:00:00Z",
           searchResult([6, 14, 29]),
           "Three. In extract-invoice (a0000000…0006), the first model read the total as 1,248.00 instead of 1,284.00, and the retry fixed it.",
         ),
         dashboardTurn("Build me a dashboard of invoices, schema-check failures and cost.", {
           name: "Invoice extraction",
           description: "Invoices read, schema-check failures and cost.",
           definition: {
             widgets: [
               widget("Invoices per day", 0, "bar", traces(BILLING_FILTER, { agg: "count", label: "Invoices" })),
               widget(
                 "Schema check failures",
                 1,
                 "number",
                 traces(BILLING_FILTER, { agg: "rate", where: "status = 'error'", label: "Failures" }),
               ),
               widget(
                 "Cost per invoice",
                 2,
                 "line",
                 traces(BILLING_FILTER, { agg: "avg", field: "cost_total", label: "Cost" }),
               ),
               widget("Invoices by model", 3, "bar_h", {
                 source: "traces",
                 filter: BILLING_FILTER,
                 measures: [{ agg: "count", label: "Invoices" }],
                 breakdown: { field: "models", limit: 5 },
               }),
             ],
           },
         }),
       ],
     },
   ];
   ```
   The `searchTurn`/`dashboardTurn` calls above may be reformatted by Biome; the strings and numbers are the contract. The `traces(...)` helper takes `(filter, measure)`; the two bar_h widgets write their query out because they add a `breakdown`.
4. **Selection.** Add after `STORIES`:
   ```ts
   /** Every story turn's question, lowercased and trimmed, with its script. */
   const SCRIPTS = new Map<string, Script>(
     STORIES.flatMap((story) =>
       story.turns.map((turn): [string, Script] => [
         turn.question.trim().toLowerCase(),
         storyScript(turn, story.heroTraceId),
       ]),
     ),
   );

   /** The script that answers `message`: a story turn's when it is that question (trimmed, any case), else today's. */
   export function scriptFor(message: string): Script {
     return SCRIPTS.get(message.trim().toLowerCase()) ?? DEFAULT_SCRIPT;
   }
   ```
   Export `Script` as a type too (`export type Script`), because `scriptFor` is exported and the test types its use.
5. **Conversations.** Replace `conversationDetail()` and `conversationList()` with:
   ```ts
   type Summary = {
     id: string;
     title: string;
     message_count: number;
     created_at: string;
     last_active_at: string;
   };

   /** The detail of a stored story chat: both turns, as they were run. */
   function storyDetail(story: Story) {
     const turns: Ran[] = story.turns.map((turn) => ({
       script: storyScript(turn, story.heroTraceId),
       context: STORY_CONTEXT,
     }));
     const messages = storedMessages(turns, story.base);
     return {
       id: story.conversationId,
       title: story.turns[0].question,
       message_count: messages.length,
       created_at: stamp(story.base, 0),
       last_active_at: stamp(story.base, messages.length),
       messages,
     };
   }

   function summaryOf(detail: Summary): Summary {
     const { id, title, message_count, created_at, last_active_at } = detail;
     return { id, title, message_count, created_at, last_active_at };
   }

   /** The new chat: no messages before its first turn, then every turn run in it. */
   function newConversationDetail() {
     const messages = storedMessages(ran, NEW_BASE);
     return {
       ...newConversation,
       title: ran[0]?.script.title ?? "",
       message_count: messages.length,
       last_active_at: messages.length === 0 ? newConversation.last_active_at : stamp(NEW_BASE, messages.length),
       messages,
     };
   }

   /** The new chat first, then the four story chats, then the fixture list. */
   function conversationList() {
     const stories = STORIES.map((story) => summaryOf(storyDetail(story)));
     const fixtures = assistantConversations.data;
     const listed = created ? [summaryOf(newConversationDetail())] : [];
     return { data: [...listed, ...stories, ...fixtures], next_cursor: null };
   }
   ```
   Rename the existing `summaryOf(spec: TurnSpec)` of step 3 to `turnSummaryOf` (two names must not clash). Remove the now unused `conversationDetail`. `newConversation` is unchanged.
6. **Handlers.** Replace `assistantHandlers` with:
   ```ts
   export function assistantHandlers(eventDelayMs: number = EVENT_DELAY_MS): HttpHandler[] {
     return [
       http.post(`${PROJECT}/conversations`, () => {
         created = true;
         ran = [];
         return HttpResponse.json(newConversation, { status: 201 });
       }),
       http.get(`${PROJECT}/conversations`, () => HttpResponse.json(conversationList())),
       http.get(`${PROJECT}/conversations/:id`, ({ params }) => {
         if (params.id === NEW_CONVERSATION_ID) {
           return HttpResponse.json(newConversationDetail());
         }
         const story = STORIES.find((item) => item.conversationId === params.id);
         return story === undefined ? undefined : HttpResponse.json(storyDetail(story));
       }),
       // patch, delete and feedback handlers: unchanged
       http.post(`${PROJECT}/conversations/:id/turns`, async ({ params, request }) => {
         const turn = await readTurnRequest(request);
         const script = scriptFor(turn.message);
         const isNew = params.id === NEW_CONVERSATION_ID;
         const turnId = isNew ? ran.length + 1 : 1;
         if (isNew) {
           ran.push({ script, context: turn.context });
         }
         return turnStream(script.events(turnId), { delayMs: eventDelayMs });
       }),
     ];
   }
   ```
   Keep the existing doc comment of `assistantHandlers` and the three unchanged handlers (patch, delete, feedback) as they are. `browser.ts` keeps calling `assistantHandlers()`.
   Before the first turn the old code answered `{ ...newConversation, messages: [] }` for the new id; `newConversationDetail()` returns the same with `title: ""` and `message_count: 0`, which `newConversation` already has.
7. **Check the leftovers.** `turnRan`, `conversationDetail` and the `TRACE_ID` constant: `TRACE_ID` stays (the default script uses it). Delete `turnRan` and anything the compiler reports unused. Run `rtk proxy grep -rn "000000570[4-9]" fixtures web/src`; the only hits in `web/src` are in `assistant.ts` (`…570801` to `…570804`) and this phase's test, plus the other M14 phases' own files; if a hit is anywhere else, stop and report it with `QUESTION:`.

**Tests** (`web/src/mocks/assistant.test.ts`, new). Setup:
```ts
import { beforeEach, expect, it } from "vitest";
import { applyEvent, bubblesOf, type AssistantBubble } from "@/lib/assistant/transcript";
import { parseDraft, parseTurnEvent, type TurnEvent } from "@/lib/assistant/types";
import { assistantHandlers, resetAssistantMock, scriptFor, STORIES } from "@/mocks/assistant";
import { createDataset } from "@/mocks/generated/dataset";
import { server } from "@/mocks/node";

const BASE = `${window.location.origin}/api/v1/projects/acme%2Fsupport-bot/assistant`;
const NEW_ID = "01900000-0000-7000-8000-000000001a02";
const DEFAULT_TRACE_ID = "0af7651916cd43dd8448eb211c80319c";

beforeEach(() => {
  resetAssistantMock();
  server.use(...assistantHandlers(0));
});
```
Helpers (write them in the test file):
- `createChat()`: `POST ${BASE}/conversations`.
- `ask(id, message, context?)`: `POST ${BASE}/conversations/${id}/turns` with JSON `{ message, context }`; reads `response.text()`, splits on `"\n\n"`, skips the `": open"` block, reads each block's `event: <name>` and `data: <json>` lines, and returns the events that `parseTurnEvent(name, data)` accepts (`TurnEvent[]`). It fails the test if one block does not parse (`expect(event).not.toBeNull()`).
- `detail(id)`: `GET ${BASE}/conversations/${id}` as JSON.
- `streamedBubble(events)`: `events.reduce(applyEvent, { role: "assistant", seqs: [], blocks: [], status: "streaming" })`, typed `AssistantBubble`.
- `comparable(bubble)`: the bubble with each step's `result` reduced to `{ ok, size_bytes, rows }` (drop `stubbed`, which only stored messages carry) and without `seqs` and `feedbackSeq`.

Cases:
1. `"picks each story's script from the question, trimmed and in any case"`: for every story and its two turns, `ask(NEW_ID, turn.question)` (after `createChat()` and `resetAssistantMock()` between stories) streams the story's hero id in `start`; with `"  " + question.toUpperCase() + "  "` the events equal those of the exact question except the `turn_id`. Turn 1 has a `tool_call` named `search_traces`; turn 2 has one named `draft_dashboard` and a `draft` event of kind `dashboard`. The final event of each is `done` with `finish_reason: "stop"`.
2. `"keeps today's script for any other question"`: `createChat()`, `ask(NEW_ID, "hello")` gives `start` `{ turn_id: 1, trace_id: DEFAULT_TRACE_ID }`; tool calls named `search_traces` then `draft_filter`; a `draft` of kind `filter`; the last text delta starts with `"Three traces failed in the last 24 hours"`. After it, `detail(NEW_ID)` has `title` `"Which trace errors spiked?"`, 6 messages with seqs 1 to 6, and message 1's text `"Which trace errors spiked in the last 24 hours?"` with context `{ page: "traces", filter: "status = 'error'", range: "24h" }`. `scriptFor("")` is the default (its `title` is the same).
3. `"stores both turns of a story as they were streamed"`, one `it` per story (`it.each(STORIES)`): `createChat()`; `first = ask(NEW_ID, q1, { page: "traces", range: "24h" })`; `second = ask(NEW_ID, q2, { page: "dashboard" })`; the `start` events carry `turn_id` 1 and 2. `detail(NEW_ID)`: `title` is `q1`, `message_count` 8, `messages` has `seq` 1 to 8 in order, the roles are `user, assistant, tool, assistant, user, assistant, tool, assistant`, message 1 and 5 have the questions as their text and the contexts that were sent. `bubblesOf(messages)` has 4 bubbles (user, assistant, user, assistant); each user bubble's `text` is the question; each assistant bubble is, after `comparable`, deep equal to `comparable(streamedBubble(events))` for its turn (blocks and `traceId` equal: the hero id; `status: "complete"`). The stored draft block of turn 2 equals the streamed `draft` event's `draft`.
4. `"names only traces that exist"`: `const ids = new Set(createDataset({ seed: 1, now: Date.parse("2026-11-19T12:00:00Z"), traces: 50 }).traces.map((trace) => trace.trace_id))`. For every story: its `heroTraceId` is in `ids`; the search turn's `result.rows` all have a `trace_id` in `ids`, and their count equals the `rows` of the stored tool result and of the `tool_result` summary (support 6, coding 1, voice 12, extraction 3); the hero is among the rows of the coding, voice and extraction searches and not among the support ones. No text part of the stored messages contains a 32-character lowercase hex string that is not in `ids` (regex `/\b[0-9a-f]{32}\b/g`). The abbreviated id written in an answer, `a0000000…000N`, matches the hero id's first 8 and last 4 characters (support has none).
5. `"lists the four story chats after a new one and before the fixture chat"`: `GET ${BASE}/conversations` returns, before `createChat()`, ids `[…570801, …570802, …570803, …570804, …1a01]`, whose `title`s are the four first questions and `message_count` 8 for the four, and `1a01` last; no `messages` key on any item. After `createChat()` the first item is `NEW_ID` with title `""` and `message_count` 0; after a turn its title is the story's question and `message_count` is 4.
6. `"opens a stored story chat directly"`: `detail(story.conversationId)` has 8 messages; the same `bubblesOf` shape as case 3; the user bubble's text is the question; the last assistant bubble has a `draft` block of kind `dashboard`, and `traceId` is the hero id. A GET of an unknown id (`01900000-0000-7000-8000-00000000ffff`) is not answered by this module: the handler returns `undefined`, so the manifest answers 404 (`expect(response.status).toBe(404)`).
7. `"drafts dashboards the assistant card accepts"`: for every story the turn-2 draft passes `parseDraft("dashboard", draft)` (not null); its widget titles equal exactly:
   - support, "Answer quality": `["Correctness by day", "Answers per day", "Cost per answer", "Answers by model"]`;
   - coding, "Code agent health": `["Failed runs per day", "Error rate", "Cost per run", "p95 duration"]`;
   - voice, "Voice latency": `["p95 reply time", "Reply time by day", "Calls over 3 s", "Calls per day"]`;
   - extraction, "Invoice extraction": `["Invoices per day", "Schema check failures", "Cost per invoice", "Invoices by model"]`;
   the widget ids equal, in order, the ids listed in the cross-phase contract above, and the draft names and descriptions equal the table of the content (`Answer quality` / `Support answers: quality, volume and cost.`, `Code agent health` / `Failed runs, cost and duration of the coding agent.`, `Voice latency` / `How long callers wait for a reply.`, `Invoice extraction` / `Invoices read, schema-check failures and cost.`). Widget ids are unique within a draft, and `layout.x + layout.w <= 60`.
8. `"a new chat starts empty"`: `createChat()`, `ask(NEW_ID, STORIES[0].turns[0].question)`, then `createChat()` again: `detail(NEW_ID).messages` is `[]` and its `message_count` 0.
9. `"does not store a turn sent to another chat"`: `createChat()`, `ask(STORIES[0].conversationId, "hello")` streams events, and `detail(NEW_ID).messages` is `[]`; `detail(STORIES[0].conversationId).message_count` is still 8.

**Done when:** from the worktree root, after `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`:
- `pnpm --dir web exec vitest run src/mocks/assistant.test.ts src/components/assistant --maxWorkers=8` passes (the panel tests use their own `server.use` scripts and must still pass);
- `pnpm --dir web run typecheck` passes;
- `./node_modules/.bin/biome check --write web/src/mocks/assistant.ts web/src/mocks/assistant.test.ts`, then `./node_modules/.bin/biome check web/src/mocks/assistant.ts web/src/mocks/assistant.test.ts` passes. Don't change any text by hand to please Biome;
- by hand (after the dev server runs with `just web-dev`, optional): in the Ask AI window, typing a story question and then its build request shows the question as the user bubble after each turn, "Searched traces" with the row count, and then "Drafted a dashboard" with the card listing the four widgets.

**Commit:** `Script the assistant's story conversations`

---

### P5 Register the story mocks

**Goal:** the browser worker serves P1's story prompts and seeds P2's story dashboards, the web checks pass once with everything merged, and the story pages render in a real browser.

**Edit:**
- `web/src/mocks/browser.ts`
- `docs/plans/website/README.md` (the M14 row only)

No other file. P5 runs on the plan's branch after P0 to P4 are merged (at least P1 and P2).

**Steps:**

1. In `web/src/mocks/browser.ts`, add two imports, in the place Biome's import order puts them (after `./sql` and `./sse`, before `./writes`: `./stories-prompts`, then `./story-dashboards`):
   ```ts
   import { storyPromptHandlers } from "./stories-prompts";
   import { storyDashboards } from "./story-dashboards";
   ```
2. Replace the line `...dashboardHandlers(seedDashboards()),` with `...dashboardHandlers([...seedDashboards(), ...storyDashboards()]),`. `seedDashboards()` is unchanged and still returns the one fixture dashboard.
3. Add `...storyPromptHandlers,` on its own line immediately before `...fallbackHandlers,` (so it comes after `...writeHandlers,`). The resulting order is: `fixtureStreamHandlers()`, `playgroundHandlers`, saved views, dashboards, `sqlHandlers`, `assistantHandlers()`, `writeHandlers`, `storyPromptHandlers`, `fallbackHandlers`, `handlers`.
4. In `docs/plans/website/README.md`, set the M14 row to exactly:
   `| M14 Prompts, dashboards, queues, assistant | [`14-m14-prompts-dashboards-queues-assistant.md`](14-m14-prompts-dashboards-queues-assistant.md) | done (P0–P5) |`
   Change nothing else in the file.
5. Run `./node_modules/.bin/biome check --write web/src/mocks/browser.ts`, then without `--write`.
6. Check the widget titles agree between P2 and P4. From the worktree root, for each of the 16 titles in the fixed content's dashboards table, run `rtk proxy grep -c '"<title>"' web/src/mocks/story-dashboards.ts web/src/mocks/assistant.ts`; every title must be found in both files (a count of at least 1 for each). Report any title that is missing from either file with `QUESTION:`.
7. Run `just check-web` once, from the worktree root, in the foreground with a long timeout, and check its exit code. It runs every web test; that is deliberate, as this is the plan's last phase. Do not run `just check`.
8. Look at the pages in a real browser (manual check):
   1. Start the dev server in the background: `pnpm --dir web exec vite --port 5288 --strictPort --host 127.0.0.1`. Wait until `curl -s -o /dev/null http://127.0.0.1:5288/` succeeds. If port 5288 is in use, stop and report it; never kill a process you did not start.
   2. Write a scratch script outside the repository (for example in your scratchpad directory, never committed). It loads Playwright from the website package (`createRequire("<worktree>/website/package.json")("playwright")`), launches `chromium`, opens `http://127.0.0.1:5288/` once and waits 3 s so the mock service worker registers (as `website/scripts/screenshots.mjs` does for its warm-up), then for each URL below calls `page.goto`, waits for `networkidle` plus 2 s, records `console` errors, saves a full-page PNG into the scratch directory, and prints the URL, the first 200 characters of `main`'s text and whether the text contains `Could not load`, `Not found`, `Something went wrong` or (dashboards) `No traces in range`.
      - the four dashboards: `/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570401`, `…570402`, `…570403`, `…570404`;
      - a story prompt's playground: `/acme/support-bot/playground?prompt=write-patch&version=3`;
      - a story queue's review page: `/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570503/review/01900000-0000-7000-8000-000000570513`;
      - a story issue: `/acme/support-bot/issues/01900000-0000-7000-8000-000000570102`.
   3. Run it and Read each PNG. Each page must render its content (four widgets with data on each dashboard; the playground with the `write-patch` prompt loaded; the review form with the `resolved` and `tone` fields; the issue's page), with no error state, and no console error that mentions the story ids.
   4. Stop the dev server you started (by the process id you recorded when you started it), and delete the scratch PNGs and script.

**Tests:** none of its own: `just check-web` runs the suite, and step 8 is the browser check.

**Done when:**
- `./node_modules/.bin/biome check web/src/mocks/browser.ts` passes;
- `just check-web` exits 0;
- step 6 finds all 16 titles in both files;
- step 8's four dashboards, playground, review page and issue page all render without an error state;
- `git status` lists only `web/src/mocks/browser.ts` and `docs/plans/website/README.md`, and no dev server of yours is still running.

**Commit:** `Register the story prompts and dashboards in the browser mock`

---

## Settled questions

1. **`support-reply` keeps its fixture labels** (`latest` and `staging` on v2). No override: the picture doesn't need `production`.
2. **Measure labels may differ between P2's dashboards and P4's draft cards.** The cards show titles only, and the titles, ids, layouts and filters agree. That is accepted.
