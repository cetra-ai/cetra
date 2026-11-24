# M13: Voice story, trace list, issues and alerts

The homepage's stories become Support agent, Coding agent, Voice agent and Document extraction (design §12, D-W-24, D-W-25). This plan does four things:
- It replaces M12's research trace with a hand-written voice-agent trace, and the hero's Research tab with a Voice tab.
- It teaches the mock trace list to apply the filter language, so a story's list can be a real filter.
- It adds each story's issue and alert rule to the large mock, tied to the story's hero trace, with ids of their own.

M14 adds the other mock objects; M15 adds the screenshots and the switcher.

**Prerequisites:** branch `claude/focused-screenshots` (PR #249) at or after the commit that adds this plan; M12 done. Playwright's Chromium for `playwright@1.65.1` is installed.

**Read first:**
- `docs/website/00-design.md` §12.
- `docs/plans/website/README.md` § Conventions for every phase. They apply here, and this plan edits `web/`, whose checks each phase names.
- `docs/plans/website/12-m12-hero-scenarios.md`, E1: how a hand-written trace is defined (`Showcase`, `story`, the payload helpers).
- `web/src/mocks/generated/dataset.ts`: `SHOWCASES`, `RESEARCH_SHOWCASE` and the constants above it, `spanId`, `rfc3339`, `visibleTraces`, `Dataset`.
- `web/src/mocks/generated/dataset.test.ts`: the scenario tests.
- `web/src/mocks/generated/widget-query.ts`: `COLUMNS`, `Unsupported`, `compile`, `traceRow`.
- `web/src/mocks/generated/handlers.ts`: `largeHandlers`, `readQuery`, `filterTraces`, `listTraces`, `listSpans`, `userIdOfFilter`, `ignoredFilter`.
- `web/src/mocks/generated/handlers.test.ts`: its setup, and "lists only one user's traces for a user_id filter, and flags other filters as ignored".
- The fixtures `fixtures/api/static/issue.json`, `issue-occurrences.json`, `issue-events.json`, `issues.json`, `alert-rules.json` and `project-alert-events.json`, for the shapes.
- `web/src/api/schema.gen.ts` for `Issue`, `IssueOccurrences`, `IssueEvent`, `AlertRule`, `AlertCondition` and `AlertEvent`.
- `website/screenshots.json` (`marketing-trace-view-research`), `website/src/marketing/home/Hero.astro`.

## What it is, and what it is not

When it's done:
- **The voice trace.** Trace index 5 of every generated dataset is `voice-agent`, 180 s old: one caller turn whose reply starts 2.8 s after the caller stops talking, because an availability check takes 1.22 s. The research trace is gone.
- **The hero.** The hero's third tab is "Voice agent", with its own screenshot, and the research screenshot is deleted.
- **The trace list.** In the large mock, `GET …/traces` applies any filter the widget query's compiler understands: `status`, `tags`, `name`, `user_id`, the numeric columns, and `and`. Filters it doesn't understand are still flagged as ignored, as today. The spans list is unchanged.
- **Issues.** The large mock serves four story issues, one per story, with ids of their own. Each has occurrences computed from the dataset: the story's hero trace first, then matching generated traces. They also have events, and they are listed before the fixture issue.
- **Alert rules.** The large mock serves four story alert rules, two of them firing, with their events. They are listed before the fixture rules.

Out of scope:
- The other mock objects: prompts, dashboards, queues and the assistant (M14).
- The chapter screenshots and the switcher (M15).
- Audio in the voice trace (design §12, non-goals).
- The facets, which still ignore filters.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-12 | Screenshots come from the web UI on its MSW mocks |
| D-W-24 | The hero shows four example traces behind tabs; the voice agent replaced the research agent |
| D-W-25 | Every homepage picture follows one of the four stories, through mock objects tied to the story's hero trace |

The voice story's trace list (design §12: sorted by duration) is not delivered here: generated voice-tagged traces run up to 20 s, so the 3.1 s hero isn't on top. M15 picks the list's URL.

The user's answers (2026-11-24):
- The stories, in order, are Support agent, Coding agent, Voice agent and Document extraction.
- Captions name an outcome the reader wants.
- There is no audio in the voice trace.
- GLM implements; Haiku takes over when GLM's limit is reached.

Planner's readings (design §12):
- Story objects live next to the generated data, served by the large handlers (`generated/`), and are reached by id. There is no story mode.
- The checked fixtures under `fixtures/api/` are not edited.
- The story handlers read the dataset's `now`, so their dates follow the page's clock like the traces do.

No `CHANGELOG.md` line: the product doesn't change.

## Versions

No new dependency.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| V1 The voice trace | `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts` | — |
| L1 Filters on the trace list | `web/src/mocks/generated/widget-query.ts`, `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts` | — |
| S1 Story issues and alert rules | `web/src/mocks/generated/stories.ts` (new), `web/src/mocks/generated/stories.test.ts` (new), `web/src/mocks/generated/handlers.ts` | V1, L1 |
| V2 The voice hero shot | `website/screenshots.json`, `website/src/marketing/home/Hero.astro`, `website/src/marketing/home/Chapters.astro` (an alt), the PNGs under `website/src/assets/screenshots/`, `docs/plans/website/README.md` | V1, S1 |

They run one after the other, in the order V1, L1, S1, V2. V2 is last so that the screenshots are captured once, from the final dataset.

---

### V1 The voice trace

**Goal:** trace index 5 is the hand-written `voice-agent` trace, and the research trace and its constants are gone.

**Edit:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts`. No other file.

**Steps:**

1. In `dataset.ts`, delete `RESEARCH_QUESTION`, `RESEARCH_ANSWER`, `RESEARCH_QUERIES`, `OWASP_CSV`, `RFC_4180` and `RESEARCH_SHOWCASE`.
2. In their place, add the code below exactly (Biome may reformat). Strings are product copy: keep every character.
   ```ts
   const VOICE_QUESTION = "Hi, can I move my dentist appointment to Friday afternoon?";
   const VOICE_ANSWER = "Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you?";

   const VOICE_SHOWCASE: Showcase = {
     index: 5,
     ageMs: 180_000,
     service: "voice-agent",
     user: "user-019",
     tags: ["voice"],
     models: ["gpt-4.1-mini", "gpt-4o-mini-transcribe", "gpt-4o-mini-tts"],
     scores: [{ name: "under_budget", source: "check", value: { data_type: "boolean", value: false } }],
     spans: [
       { name: "voice-agent", kind: "agent", from: 0, to: 3_100 },
       { name: "detect end of turn", kind: "span", parent: 0, from: 0, to: 120 },
       { name: "speech to text", kind: "llm", parent: 0, from: 130, to: 520, model: "gpt-4o-mini-transcribe", provider: "openai", inputTokens: 310, outputTokens: 22, cost: 0.0003 },
       { name: "detect intent", kind: "llm", parent: 0, from: 530, to: 880, model: "gpt-4.1-mini", provider: "openai", inputTokens: 610, outputTokens: 14, cost: 0.00027, ttftMs: 210 },
       { name: "check_availability", kind: "tool", parent: 0, from: 890, to: 2_110 },
       { name: "draft reply", kind: "llm", parent: 0, from: 2_120, to: 2_640, model: "gpt-4.1-mini", provider: "openai", inputTokens: 980, outputTokens: 46, cost: 0.00047, ttftMs: 180 },
       { name: "safety check", kind: "guardrail", parent: 0, from: 2_650, to: 2_700 },
       { name: "text to speech", kind: "llm", parent: 0, from: 2_710, to: 3_100, model: "gpt-4o-mini-tts", provider: "openai", inputTokens: 46, outputTokens: 820, cost: 0.0098, ttftMs: 90 },
     ],
     story: {
       inputPreview: VOICE_QUESTION,
       outputPreview: VOICE_ANSWER,
       payloads: [
         { input: textPayload(VOICE_QUESTION), output: textPayload(VOICE_ANSWER) },
         {
           input: jsonPayload({ speech_ms: 2_400, silence_ms: 600 }),
           output: jsonPayload({ end_of_turn: true }),
         },
         {
           input: jsonPayload({ audio: "caller-turn-3.wav", duration_ms: 2_400, language: "en" }),
           output: textPayload(VOICE_QUESTION),
         },
         chatPayload(
           "Classify the caller's request: book, reschedule, cancel or question. Reply with JSON.",
           VOICE_QUESTION,
           '{"intent": "reschedule", "day": "friday", "part": "afternoon"}',
         ),
         toolPayload(
           "check_availability",
           { clinic: "north", day: "friday", after: "12:00" },
           jsonPayload({ slots: ["14:00", "16:30"], latency_ms: 1_220 }),
         ),
         chatPayload(
           "You are a receptionist on the phone. Answer in one or two short sentences.",
           `${VOICE_QUESTION}\n\nFree slots: 14:00, 16:30`,
           VOICE_ANSWER,
         ),
         {
           input: jsonPayload({ text: VOICE_ANSWER }),
           output: jsonPayload({ passed: true, reasons: [] }),
         },
         {
           input: textPayload(VOICE_ANSWER),
           output: jsonPayload({ audio: "reply-turn-3.wav", duration_ms: 3_900, first_audio_ms: 90 }),
         },
       ],
     },
   };
   ```
3. In `SHOWCASES`, replace `RESEARCH_SHOWCASE` with `VOICE_SHOWCASE`, at the same position.
4. `rtk proxy grep -n -i "research\|OWASP\|RFC_4180" web/src/mocks/generated/dataset.ts` prints nothing.
5. **Tests** (`dataset.test.ts`):
   - **Delete** "runs the research agent's five searches in parallel".
   - **Add** `"makes the voice agent's availability check its slowest span"`, using `scenarioOf(5)`:
     - the trace's `name` is `voice-agent`;
     - its duration is 3,100 ms (`duration_ns` 3,100,000,000);
     - among the non-root spans, the one with the largest `duration_ns` is the one named `check_availability`, at index 4 of `spans` counting the root;
     - its span id is `"0000000500000005"`.
   - **Add** `"scores the voice agent's reply over its latency budget"`: trace 5's `scores` has one score named `under_budget`, whose value is `{ data_type: "boolean", value: false }`.
   - The existing generic tests ("makes traces 4 to 6 the hand-written scenario traces", the nesting and payload tests) cover the voice trace through `SHOWCASES` and must pass unchanged.
   - If one of them names the research trace, change only that name to `voice-agent`.

**Done when:** from the worktree root, after `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`:
- `pnpm --dir web exec vitest run src/mocks --maxWorkers=8` passes;
- `pnpm --dir web run typecheck` passes;
- `./node_modules/.bin/biome check web/src/mocks/generated/dataset.ts web/src/mocks/generated/dataset.test.ts` passes, after a first run with `--write`. Don't change any text by hand to please Biome.

**Commit:** `Replace the research scenario trace with a voice agent`

---

### L1 Filters on the trace list

**Goal:** in the large mock, `GET …/traces` applies every filter the widget query's compiler can evaluate, and flags only the others as ignored.

**Edit:** `web/src/mocks/generated/widget-query.ts`, `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts`. No other file.

**Steps:**

1. In `widget-query.ts`, add and export, after `compile`:
   ```ts
   /**
    * A predicate over traces for `filter`, or undefined when the mock cannot evaluate it. The trace
    * list uses it (M13), with the same columns as a traces widget.
    */
   export function tracePredicate(filter: string): ((trace: Trace) => boolean) | undefined {
     try {
       const predicate = compile(filter, COLUMNS.traces, "filter");
       return (trace) => predicate(traceRow(trace));
     } catch (error) {
       if (error instanceof Unsupported) {
         return undefined;
       }
       throw error;
     }
   }
   ```
   `compile`, `parseChip` and `splitConjuncts` throw only `Unsupported` for any text `readQuery` lets through: text they can't model becomes a `raw` chip that `predicateOf` refuses. Don't widen the `catch`. List containment is written `tags @> ['x']` in the filter language (spec 05); `tags contains 'x'` is not a filter.
2. In `handlers.ts`:
   1. Import `tracePredicate` from `@/mocks/generated/widget-query`, next to `widgetQueryHandler`.
   2. Give `filterTraces` a third parameter: `predicate?: (trace: Schema<"TraceSummary">) => boolean`.
   3. When `predicate` is set, `filterTraces` keeps a trace only if `predicate(trace)` is true, and skips the `user_id` check; `userIdOfFilter` is then not consulted. When it's unset, `filterTraces` behaves exactly as today.
   4. In `listTraces`:
      ```ts
      const predicate = parsed.filter ? tracePredicate(parsed.filter) : undefined;
      const rows = sortTraces(filterTraces(dataset.traces, parsed, predicate), parsed.sort, parsed.order);
      const filtered = predicate !== undefined || userIdOfFilter(parsed.filter) !== undefined;
      return page(rows, parsed, filtered ? {} : ignoredFilter(parsed));
      ```
   5. `listSpans` and every other caller of `filterTraces` pass no predicate. A spans filter names span columns, which the traces predicate doesn't model.
3. **Tests** (`handlers.test.ts`):
   - **Change** "lists only one user's traces for a user_id filter, and flags other filters as ignored". Its second request now uses the filter `metadata['plan'] = 'pro'` instead of `user_id = 'a' and name = 'b'`. The map key is a filter the mock can't evaluate, so the response is still flagged. Keep the rest of the test.
   - **Add** `"applies a status filter to the trace list"`:
     - dataset `createDataset({ seed: 1, now: NOW, traces: 400 })`, `server.use(...largeHandlers(dataset))`;
     - `GET …/traces?filter=status = 'error'&limit=1000` returns exactly the visible traces whose `status` is `"error"` (compare sorted ids);
     - the `X-Mock-Filter-Ignored` header is null.
   - **Add** `"applies a tags filter combined with a name filter"`: the filter `tags @> ['retry'] and name = 'coding-agent'` returns exactly trace 4's id, without the ignored header.
   - **Add** `"sorts a filtered list"`: the filter `tags @> ['voice']` with `sort=duration&order=desc` returns traces whose `duration_ns` doesn't increase, and every one has `voice` in its tags.

**Done when:**
- `pnpm --dir web exec vitest run src/mocks 'src/routes/$org/$project/traces' --maxWorkers=8` passes;
- `pnpm --dir web run typecheck` passes;
- Biome is clean on the three files, after `--write`.

If a test outside `handlers.test.ts` fails because it relied on filters being ignored, stop and report it with `QUESTION:`. Don't edit that test.

**Commit:** `Apply filters on the mock trace list`

---

### S1 Story issues and alert rules

**Goal:** the large mock serves one issue and one alert rule per story, with occurrences, events and rule events, tied to the story's hero trace.

**Edit:** `web/src/mocks/generated/stories.ts` (new), `web/src/mocks/generated/stories.test.ts` (new), `web/src/mocks/generated/handlers.ts` (only to register the story handlers). No other file.

**Contracts.**
- All ids end with a group starting `00000057`, which no fixture uses. Check with `rtk proxy grep -rn "00000057" fixtures web/src` before you start; it must print nothing.
- Times are computed from `dataset.now` with `rfc3339` (from `dataset.ts`).
- Define a local helper `ago(dataset: Dataset, ms: number): string` returning `rfc3339(dataset.now - ms)`; the tables below write `ago(ms)` for it.
- Compare times as epoch ms (`Date.parse`). "Newest first" breaks ties by `trace_id`, descending.
- "UTC midnight" of a time `ms` is `Math.floor(ms / DAY_MS) * DAY_MS`.
- Export every function this phase names (`storyOccurrences`, `storyIssue`, `storyIssueEvents`, `storyAlertRule`, `storyAlertEvent`, `storyHandlers`); the tests import them.
- The generated occurrences are illustrative: generated traces carry generic names (`support-router`, `refund-agent`, …) and whole-trace durations, so issue 3 matches voice-tagged traces over 3 s although its title says 800 ms. That is accepted; don't try to make them agree.

Put this at the top of `stories.ts` exactly (Biome may reformat):
```ts
import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { type Dataset, rfc3339, visibleTraces } from "@/mocks/generated/dataset";
import alertRulesFixture from "../../../../fixtures/api/static/alert-rules.json";
import issuesFixture from "../../../../fixtures/api/static/issues.json";
import projectAlertEventsFixture from "../../../../fixtures/api/static/project-alert-events.json";

/**
 * The homepage stories' issues and alert rules (design §12, M13), next to the generated traces.
 * Each is tied to its story's hand-written trace (`SHOWCASES`), and has an id of its own, so the
 * fixtures keep theirs. Only the large mock serves them.
 */

const PROJECT = "/api/v1/projects/:project";
const PROJECT_ID = "01900000-0000-7000-8000-000000000001";
const TEAM_SLACK = "01900000-0000-7000-8000-000000000500";
const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;
/** Occurrences count over the 14 days before now, as the issue page asks for. */
const OCCURRENCE_WINDOW_MS = 14 * DAY_MS;
/** Example traces after the hero trace. */
const GENERATED_EXAMPLES = 4;

type Trace = Schema<"TraceSummary">;

type StoryIssue = {
  id: string;
  eventId: string;
  sourceId: string;
  sourceName: string;
  kind: Schema<"IssueSourceKind">;
  title: string;
  description: string;
  /** The story's hand-written trace: its dataset index, the span the issue is about, and why. */
  hero: { index: number; spanId?: string; reason: string };
  /** The generated traces that are occurrences too. */
  matches: (trace: Trace) => boolean;
  /** The reason shown for a generated example; absent shows the trace id. */
  reasonOf?: (trace: Trace) => string | undefined;
};

export const STORY_ISSUES: readonly StoryIssue[] = [
  {
    id: "01900000-0000-7000-8000-000000570101",
    eventId: "01900000-0000-7000-8000-000000570111",
    sourceId: "01900000-0000-7000-8000-000000570301",
    sourceName: "Topics",
    kind: "topic",
    title: "Customers ask how to export traces to CSV",
    description: "Questions about exporting traces, from the Traces page or through the API.",
    hero: { index: 3, reason: "How can I export my traces to a CSV file?" },
    matches: (trace) => trace.input_preview === "How can I export my traces to a CSV file?",
    reasonOf: (trace) => trace.input_preview,
  },
  {
    id: "01900000-0000-7000-8000-000000570102",
    eventId: "01900000-0000-7000-8000-000000570112",
    sourceId: "01900000-0000-7000-8000-000000570302",
    sourceName: "Errors",
    kind: "error",
    title: "run_tests failed: 1 failed, 47 passed",
    description: "pytest reports failures after the agent's first patch.",
    hero: { index: 4, spanId: "0000000400000009", reason: "1 failed, 47 passed" },
    matches: (trace) => trace.status === "error" && trace.tags.includes("retry"),
  },
  {
    id: "01900000-0000-7000-8000-000000570103",
    eventId: "01900000-0000-7000-8000-000000570113",
    sourceId: "01900000-0000-7000-8000-000000570303",
    sourceName: "Latency budget",
    kind: "score",
    title: "Reply started more than 800 ms after the caller",
    description: "The time from the end of the caller's turn to the first audio of the reply.",
    hero: { index: 5, spanId: "0000000500000005", reason: "check_availability took 1.22 s" },
    matches: (trace) => trace.tags.includes("voice") && trace.duration_ns > 3_000_000_000,
  },
  {
    id: "01900000-0000-7000-8000-000000570104",
    eventId: "01900000-0000-7000-8000-000000570114",
    sourceId: "01900000-0000-7000-8000-000000570304",
    sourceName: "Checks",
    kind: "score",
    title: "Invoice total doesn't match its line items",
    description: "The schema check fails when the extracted total differs from the sum of the line items.",
    hero: { index: 6, spanId: "0000000600000005", reason: "Total 1,248.00, line items 1,284.00" },
    matches: (trace) => trace.status === "error" && trace.tags.includes("billing"),
  },
];

type StoryRule = {
  id: string;
  eventId: string;
  deliveryId: string;
  name: string;
  condition: Schema<"AlertCondition">;
  state: "firing" | "ok";
  lastValue: number;
  lastCount: number;
  /** When it last fired (firing), or resolved (ok), before now. */
  changedAgoMs: number;
};

export const STORY_RULES: readonly StoryRule[] = [
  {
    id: "01900000-0000-7000-8000-000000570201",
    eventId: "01900000-0000-7000-8000-000000570211",
    deliveryId: "01900000-0000-7000-8000-000000570221",
    name: "Answer correctness under 0.7 on support-bot",
    condition: { metric: "score", window: "1h", comparator: "lt", threshold: 0.7, min_count: 10, score_name: "correctness", aggregation: "avg", filter: "tags @> ['email']" },
    state: "ok",
    lastValue: 0.86,
    lastCount: 42,
    changedAgoMs: 3 * HOUR_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570202",
    eventId: "01900000-0000-7000-8000-000000570212",
    deliveryId: "01900000-0000-7000-8000-000000570222",
    name: "Error rate above 5% on code-agent",
    condition: { metric: "error_rate", window: "15m", comparator: "gt", threshold: 0.05, min_count: 20, filter: "tags @> ['retry']" },
    state: "firing",
    lastValue: 0.083,
    lastCount: 36,
    changedAgoMs: 6 * MINUTE_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570203",
    eventId: "01900000-0000-7000-8000-000000570213",
    deliveryId: "01900000-0000-7000-8000-000000570223",
    name: "Slow replies on voice-agent",
    condition: { metric: "latency_p95", target: "trace", window: "15m", comparator: "gt", threshold: 1_500, min_count: 20, filter: "tags @> ['voice']" },
    state: "firing",
    lastValue: 2_800,
    lastCount: 54,
    changedAgoMs: 4 * MINUTE_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570204",
    eventId: "01900000-0000-7000-8000-000000570214",
    deliveryId: "01900000-0000-7000-8000-000000570224",
    name: "Schema check pass rate under 97%",
    condition: { metric: "score", window: "1h", comparator: "lt", threshold: 0.97, min_count: 20, score_name: "schema_valid", aggregation: "pass_rate", filter: "tags @> ['billing']" },
    state: "ok",
    lastValue: 0.985,
    lastCount: 210,
    changedAgoMs: 5 * HOUR_MS,
  },
];
```
If `Schema<"AlertCondition">` refuses a field as written (for example `target`), stop and report it with `QUESTION:`.

**Steps:**

1. **Occurrences.** Write `export function storyOccurrences(dataset: Dataset, issue: StoryIssue): Schema<"IssueOccurrences">`. If the schema's name differs, use the type of the `…/issues/{id}/occurrences` 200 response, and report the name.
   1. `hero` is `dataset.traces[issue.hero.index]`. If it is undefined (a dataset with fewer traces), there is no hero example.
   2. `generated` is the traces of `visibleTraces(dataset.traces, false, false)` that:
      - are not the hero trace;
      - satisfy `issue.matches`;
      - start within `[dataset.now - OCCURRENCE_WINDOW_MS, dataset.now]`.

      Sort them newest first.
   3. `matched` is the hero trace (if any) followed by `generated`.
   4. `days`: group `matched` by the UTC day of `start_time` and keep only days with at least one trace, in ascending day order. Each day is `{ day: rfc3339(<UTC midnight of that day>), count, users }`, where `users` is the number of distinct non-null `user_id`s that day.
   5. `examples`:
      - The hero first: `{ trace_id, created_at: hero.start_time, reason: issue.hero.reason, issue_id: issue.id }`, plus `span_id: issue.hero.spanId` when set.
      - Then the first `GENERATED_EXAMPLES` of `generated`: `{ trace_id, created_at: start_time, issue_id }`, plus `reason` when `issue.reasonOf` returns a string.
   6. Return `{ days, examples }`.
2. **Issue.** Write `export function storyIssue(dataset: Dataset, issue: StoryIssue): Schema<"Issue">`, from `storyOccurrences`' `matched` traces. Factor `matched` into a private helper that both functions call. The fields:

   | Field | Value |
   |---|---|
   | `id`, `source_id`, `source_name`, `kind`, `title`, `description` | from `issue` |
   | `project_id` | `PROJECT_ID` |
   | `score_name` | `` `issue:${issue.id}` `` |
   | `state`, `regressed`, `merged_from` | `"open"`, `false`, `[]` |
   | `first_seen`, `last_seen` | the oldest and newest `start_time` of `matched` |
   | `created_at`, `updated_at` | `first_seen` and `last_seen` |
   | `count` | `matched.length` |
   | `users` | the distinct non-null `user_id`s of `matched` |
3. **Issue events.** `storyIssueEvents(dataset, issue): Schema<"IssueEventPage">` returns `{ data: [{ id, kind: "created", at: first_seen }] }`. `id` is `issue.eventId`.
4. **Alert rules.** `storyAlertRule(dataset, rule): Schema<"AlertRule">` builds:

   | Field | Value |
   |---|---|
   | `id`, `name`, `condition`, `state` | from `rule` |
   | `enabled` | `true` |
   | `channel_ids` | `[TEAM_SLACK]` |
   | `cooldown_s` | `3600` |
   | `notify_while_firing` | `false` |
   | `last_value`, `last_count` | `rule.lastValue`, `rule.lastCount` |
   | `last_evaluated_at` | `ago(MINUTE_MS)` |
   | `last_fired_at` | `ago(rule.changedAgoMs)` when firing, else omitted |
   | `created_at`, `updated_at` | `ago(7 * DAY_MS)` |
5. **Alert events.** `storyAlertEvent(dataset, rule): Schema<"AlertEvent">` is one event:

   | Field | Value |
   |---|---|
   | `id` | `rule.eventId` |
   | `rule_id` | `rule.id` |
   | `kind` | `"fired"` when firing, else `"resolved"` |
   | `value`, `count` | `rule.lastValue`, `rule.lastCount` |
   | `threshold`, `comparator`, `metric`, `window` | from `rule.condition` |
   | `window_to`, `created_at` | `ago(rule.changedAgoMs)` |
   | `window_from` | `window_to` minus the window's length (`1m`, `5m`, `15m`, `1h`, `6h`, `24h`) |
   | `deliveries` | one: `{ id: rule.deliveryId, channel_id: TEAM_SLACK, channel_name: "Team Slack", channel_kind: "slack", status: "sent", attempts: 1, updated_at: created_at }` |

   Use the schema's event type; if its name differs from `AlertEvent`, use the item type of the `…/alert-events` 200 response, and report it.
6. **Handlers.** `export function storyHandlers(dataset: Dataset): HttpHandler[]` returns:
   - `GET ${PROJECT}/issues`: `{ data: [...STORY_ISSUES.map(storyIssue), ...issuesFixture.data], next_cursor: null }`. It ignores the query (state, sort, cursor), as the manifest does.
   - `GET ${PROJECT}/issues/:id`, `…/occurrences` and `…/events`: the story issue with that id; for any other id return `undefined`, so the request falls through to the fixtures.
   - `GET ${PROJECT}/alert-rules`: `{ data: [...STORY_RULES.map(storyAlertRule), ...alertRulesFixture.data], next_cursor: null }`.
   - `GET ${PROJECT}/alert-rules/:id`: the story rule, else `undefined`.
   - `GET ${PROJECT}/alert-rules/:id/events`: `{ data: [storyAlertEvent], next_cursor: null }` for a story rule, else `undefined`.
   - `GET ${PROJECT}/alert-events`: `{ data: [...the four story events newest first, ...projectAlertEventsFixture.data], next_cursor: null }`. It ignores the query.

   Check every path against `web/src/api/schema.gen.ts`'s `paths`. If one differs, use the real one and report it.
7. In `handlers.ts`, import `storyHandlers` and put `...storyHandlers(dataset)` first in `largeHandlers`' array.
8. **Tests** (`stories.test.ts`, with `const NOW = Date.parse("2026-06-01T00:00:00.000Z")` and `createDataset({ seed: 1, now: NOW, traces: 2000 })`):
   - `"makes the hero trace the first example of every story issue"`: for each `STORY_ISSUES` entry, `examples[0]` has the hero trace's `trace_id`, the issue's `reason`, and `span_id` when `hero.spanId` is set. That span id exists in the hero trace's detail (`dataset.traceDetail`).
   - `"counts an issue's occurrences the same way in the issue and its days"`: for each story issue:
     - `issue.count` equals the sum of `days[].count`;
     - `issue.users` is at least every day's `users`;
     - `first_seen` is at most `last_seen`;
     - every example's `trace_id` is in the dataset.
   - `"gives every story issue at most five examples, the generated ones newest first"`.
   - `"lists the story rules with their state and a fired event for the firing ones"`:
     - the four `storyAlertRule`s have the states `ok`, `firing`, `firing` and `ok`, in that order;
     - each `storyAlertEvent` kind matches its rule's state;
     - `window_to` minus `window_from` equals the window.
   - `"serves the story issues and rules before the fixtures, and lets other ids fall through"`. Use the handler setup of `handlers.test.ts`: `server.use(...storyHandlers(dataset))`, and `PROJECT` built from `location.origin`.
     - `GET …/issues` starts with the four story issue ids, then the fixture issue's id.
     - `GET …/alert-rules` starts with the four story rule ids.
     - `GET …/issues/01900000-0000-7000-8000-000000570102/occurrences` answers 200.
     - `GET …/alert-rules/01900000-0000-7000-8000-000000570203` answers the voice rule.
     - `GET …/issues/01900000-0000-7000-8000-000000001c01` is not answered by the story handlers. Check that it reaches the fixture handlers and answers the fixture issue. If the test server doesn't serve the manifest handlers, stop and ask.

**Done when:**
- `pnpm --dir web exec vitest run src/mocks 'src/routes/$org/$project/issues' 'src/routes/$org/$project/alerts' --maxWorkers=8` passes;
- `pnpm --dir web run typecheck` passes;
- Biome is clean on the three files, after `--write`.

If an existing test fails because the large handlers now answer issues or alert rules, stop and report it with `QUESTION:`. Don't edit that test.

**Commit:** `Add each story's issue and alert rule to the large mock`

---

### V2 The voice hero shot

**Goal:** the hero's third tab shows the voice trace, and every screenshot reflects M13's dataset.

**Edit:**
- `website/screenshots.json` (the `marketing-trace-view-research` entry);
- `website/src/marketing/home/Hero.astro` (the third scenario);
- `website/src/marketing/home/Chapters.astro` (the Alert chapter's alt only);
- the PNGs under `website/src/assets/screenshots/`: delete `marketing-trace-view-research.png`, add `marketing-trace-view-voice.png`, and keep the rewritten others;
- `docs/plans/website/README.md` (the M13 row only).

No other file.

**Steps:**
1. In `screenshots.json`, rename `marketing-trace-view-research` to `marketing-trace-view-voice`, and change its first action's text from `research-agent` to `voice-agent`. Keep the rest.
2. `git rm website/src/assets/screenshots/marketing-trace-view-research.png`.
3. In `Hero.astro`:
   1. Replace the `traceResearch` import with `import traceVoice from "../../assets/screenshots/marketing-trace-view-voice.png";`.
   2. Replace the third `SCENARIOS` entry with this, exactly:
   ```ts
   {
     tab: "Voice agent",
     src: traceVoice,
     label: "Traces · Voice agent",
     alt: "The cetra trace view: a voice agent's trace for one caller turn, where a slow availability check makes the reply start 2.8 s after the caller stops talking, with its timeline, models and costs.",
     caption: "Find the one slow tool call behind a reply that started 2.8 s late.",
   },
   ```
4. Run `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots` for every shot, in the foreground with a long timeout. The dataset changed, so the lists, sessions, users and dashboards move a little.
5. Look at `marketing-trace-view-voice.png` with the Read tool. It must:
   - be 2398 px wide;
   - show `voice-agent` in the Timeline view, with the root span selected;
   - show the 8 spans by name, `check_availability` the longest bar after the root;
   - show the root's Output card whole, bottom border included;
   - show no truncation badge.

   The alerts shots (`alerts.png`, `marketing-alerts.png`) list the four story rules with the fixture ones (the page groups them by state: three firing, the fixture's "High error rate" and two story rules), and the story events; the docs shots' sidebar badges may show more firing alerts and issues. That is expected.

   Look at the other changed PNGs by eye. They may change only in rows, counts and charts. Report any that changed another way. `git status` lists most of them, because the bytes change anyway.
6. In `Chapters.astro`, set the Alert chapter's Frame `alt` to `"The alerts page with its alert rules, three of them firing, and the recent alert events."` (it said three rules, one firing; M15 rewrites the alts per story).
7. In `docs/plans/website/README.md`, set the M13 row's status cell to: done (V1, L1, S1, V2)
8. Run `just check-web`, then `just check-docs`, one at a time, and check each exit code. `just check-web` runs every web test; that is deliberate, as this is the plan's last phase.

**Done when:** both checks pass and step 5 holds.

**Commit:** `Show the voice agent in the hero`
