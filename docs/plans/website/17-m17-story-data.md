# M17 Story data

The story pictures of M18 show numbers: "62% of the downvoted answers", "18% of runs", "p95 3.2 s", "38% re-extracted", "214 conversations this week". This milestone makes the mock data carry them. The generator gets one new hook, `marks`, that sees every generated trace with its start time and returns changes for some of them (tags, release, scores, an error, extra spans, a turn, a cost factor, a start or a duration). Each story gets a story profile that adds its marks to the M16 profile, and a `facts` function that computes the pictures' numbers from a dataset, with a test that pins them. The support project also gets the hand-written conversation of four turns.

**Prerequisites:** M16 done (branch `claude/focused-screenshots`, head d9cec46 or later).

**Read first:**
- `docs/website/00-design.md` § 13 (the table of the 24 pictures, its rules, the answers of 2026-11-25, the milestones M17-M19).
- `docs/plans/website/16-m16-story-projects.md` (the `Profile` contract, P1).
- `web/src/mocks/generated/dataset.ts`: `Showcase` (60-107), `showcaseAt` (611), `Profile` (666-708), `callsOf` (~720), `createDataset` (749-800), `traceSummary` (~880), `showcaseSummary` (967), `buildDetail` (1028), `showcaseDetail` (1051), `layoutSpans` (1125), `treeSpan` (1204), `SpanContext` (1267), `SUPPORT_TURNS` (1281), `SUPPORT_PROFILE` (1534), `spanFromSummary` (1710), `scoresFor` (2344), `scoreUuid`, `percentile`, `visibleTraces`.
- `web/src/mocks/generated/profiles.ts` (the three M16 profiles) and `profiles.test.ts` (helpers).
- `web/src/main.tsx` (the large-mock block), `web/src/mocks/projects.ts` (`STORY_PROJECTS`).
- The survey `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/m17-code-facts.md` § 0.1, 0.2 and 7.

## What it is, and what it is not

When it's done, the dev UI's four projects show the patterns below, and `facts` functions return the numbers M18's pictures and captions quote, each pinned by a test at the screenshot clock (`NOW = 2026-11-19T12:00:00Z`, seed 1, support 5,000 traces, the others 2,000).

Out of scope (M18): the story objects that show these patterns (issues, alert rules, queues, evaluators, assistant chats, prompts, the invoice dataset and experiment), the screenshots, the alts and captions. Out of scope (M19): the three pictures waiting for UI features.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-26 | A picture per story; every number a picture shows is backed by the story project's data (design § 13 rules) |
| P-1 (planner) | One generator hook, `Profile.marks`, called once per dataset with all generated summaries after the sessions are timed; it returns a `Mark` per trace index. Marks never draw from the trace's own rng stream, so a profile without marks generates exactly what it does today. |
| P-2 (planner) | `SUPPORT_PROFILE` stays as it is (no marks, the default of `createDataset`, the hash test). The dev UI uses four **story profiles**, one per story module, each `{ ...m16Profile, marks, showcases }`. |
| P-3 (planner) | Each story module exports `facts(dataset)`; M18 writes its picture texts from the values these tests pin. |
| P-4 (planner) | Where design § 13's numbers can't all hold at once, the plan keeps the question and adjusts a share, and says so: the coding cost "triples" with 18% of runs, so those runs make 73% of the cost, not 64% (0.18 × f + 0.82 = 3 gives f = 12.1, and 0.18 × 12.1 / 3 = 0.73). The extraction alert threshold becomes "more than 8 in 1 hour" (3% of about 46 invoices an hour is about 1.4, so the last hour carries a burst). M18 writes these numbers. |
| P-5 (planner) | "Since Tuesday 14:00" is `now − 46 h`; "since Monday" is `now − 84 h`; "yesterday" for the coding cost step is `now − 14 h` (Wednesday 22:00 at the screenshot clock), inside the alert chart's last 24 hours. |

## Versions

No new dependency.

## Phases

| Phase | Edit | Needs |
|---|---|---|
| P1 The marks hook | `generated/dataset.ts`, `generated/dataset.test.ts` | — |
| P2 Support story | new `generated/story-support.ts`, new `generated/story-support.test.ts` | P1 |
| P3 Coding story | new `generated/story-coding.ts`, new `generated/story-coding.test.ts` | P1 |
| P4 Voice story | new `generated/story-voice.ts`, new `generated/story-voice.test.ts` | P1 |
| P5 Extraction story | new `generated/story-extraction.ts`, new `generated/story-extraction.test.ts` | P1 |
| P6 Wire the story profiles | `web/src/main.tsx`, `generated/handlers.ts` (score definitions only), `generated/handlers.test.ts` | P2-P5 |

Paths under `web/src/mocks/` unless they start with `web/`. P2-P5 run in parallel (disjoint new files) in the session's worktree; each `git add`s its new files and commits with `git commit -- <files>`. Run `pnpm typecheck` only after the other parallel phases have committed, or one after the other.

**Checks in every phase:** `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`; from `web/`: `node_modules/.bin/vitest run <the phase's test files> --maxWorkers=4`, `pnpm typecheck`, `../node_modules/.bin/biome check --write <files>` then without `--write`. Never the full suite, `just check` or a build. Delete `web/.vitest/` if a run creates it.

---

## P1 The marks hook

**Goal:** a profile can change chosen generated traces after their start times are known, and a showcase can belong to a session.

**Edit:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts`.

### Contract (exported from `dataset.ts`)

```ts
/** A span a mark adds under the root, after the generated spans (M17). */
export type MarkedSpan = {
  name: string;
  kind: Schema<"SpanKind">;
  /** Offset from the trace's start and length, ms. */
  fromMs: number;
  durationMs: number;
  input?: Schema<"Payload">;
  output?: Schema<"Payload">;
  /** Absent: ok. */
  error?: { message: string; stacktrace: string };
};

/** What a profile's `marks` changes on one generated trace (M17). Every field is optional. */
export type Mark = {
  /** Added to the trace's tags (the result stays sorted and unique). */
  tags?: readonly string[];
  release?: string;
  /** Replace the trace's scores with the same name, and add the others. */
  scores?: readonly { name: string; source: Schema<"ScoreSource">; value: Schema<"ScoreValue"> }[];
  /** The trace's turn (its previews and its generated LLM payloads) instead of the drawn one. */
  turn?: Turn;
  /** The trace becomes an error trace: `count` error spans, named `spanName`, with this message. */
  error?: { count: number; spanName: string; message: string; stacktrace: string };
  /** Input and output tokens are multiplied by it (rounded down), so the cost is too. */
  costFactor?: number;
  /** A new start time; the end moves with it. */
  startMs?: number;
  /** A new duration; marked spans that end later extend it to their end. */
  durationMs?: number;
  spans?: readonly MarkedSpan[];
};

export type MarksContext = {
  seed: number;
  now: number;
  /** True for a showcase index: marks must not choose it, and a mark returned for it is ignored. */
  isShowcase: (index: number) => boolean;
};

// Added to `Profile`:
//   /** Changes for chosen generated traces, by index; absent: none (M17). */
//   marks?: (traces: readonly Schema<"TraceSummary">[], context: MarksContext) => ReadonlyMap<number, Mark>;

// Added to `Showcase`:
//   /** The showcase's session id (M17). */
//   session?: string;
```

`scoreUuid`, `percentile`, `visibleTraces`, `SUPPORT_TURNS`, `Turn`, `Showcase` and the showcase constants stay exported. Also export `spanId` (it builds a span id from a trace index and a span position).

### Steps

1. `Showcase.session`: `showcaseSummary` sets `session_id` to it when present. `callsOf` already skips showcase indices.
2. In `createDataset`, after `spreadSessionTurns` and before `sessionSlots`: if `profile.marks` is set, call it once with the timed summaries (all of them, showcases included) and `{ seed, now, isShowcase }`. Keep the result as `marks: ReadonlyMap<number, Mark>`, dropping entries for showcase indices.
3. Apply each mark to its summary, in this order: `startMs` (shift `start_time` and `end_time`), `durationMs` (set `duration_ns`, `end_time`), `spans` (raise the duration to the latest `fromMs + durationMs` when longer; add `spans.length` to `span_count`; add each marked span's error to `error_count`; `llm_count` += marked spans of kind `llm`), `error` (`status: "error"`, `error_count` = max(current, `count`) plus the marked spans' errors), `costFactor` (multiply `input_tokens` and `output_tokens`, floor each; recompute `total_tokens` and `cost_total` with the profile's price formula), `tags` (union, sorted), `release`, `scores` (replace by name; new scores get `score_id = scoreUuid(index, 10 + position)`). Then `sessionSlots` and the previews run as today, except that a marked `turn` replaces `turnAt(index)` for that trace.
4. `buildDetail(seed, index, trace, profile, mark)`: generate `trace.span_count - (mark?.spans?.length ?? 0)` spans as today (the same rng stream), then:
   - with `mark.error`, the generated error spans (the first `count` after the root, as `errorSpan` picks today) get `name = mark.error.spanName` and `status_message = mark.error.message`, and their exception event's stack trace is `mark.error.stacktrace` (pass it through `SpanContext`, which gets `mark?: Mark`);
   - each marked span is appended as a child of the root, in order: `span_id = spanId(index, position)` with `position` after the generated ones, `start_time = trace start + fromMs`, its kind, name, status (error with `status_message` when `error` is set), duration; `spanFromSummary` returns the marked span's `input`/`output` instead of the generated payloads when they are set.
   - `createDataset` passes `marks.get(index)` to `buildDetail` and keeps the marked turn for `spanFromSummary`.
5. Nothing else changes: a profile without `marks` produces exactly today's output.

### Tests (`dataset.test.ts`)

- The hash test stays and passes unchanged (`SUPPORT_PROFILE` has no marks).
- `a mark changes one trace and nothing else`: a test profile `{ ...SUPPORT_PROFILE, showcases: [], marks: () => new Map([[11, { tags: ["x"], release: "v9", costFactor: 2 }]]) }` against the same profile without marks: trace 11 has tag `x`, release `v9`, doubled tokens and the matching cost; every other trace is deep-equal.
- `a marked error renames the error spans`: `error: { count: 2, spanName: "ocr", message: "OCR returned no text", stacktrace: "…" }` on a trace with at least 4 spans: the trace is an error with `error_count` 2; its detail has exactly two error spans named `ocr` with that `status_message`.
- `marked spans are appended under the root`: two marked spans (one `tool` with an `output`, one with an `error`): the detail has `span_count` spans, the two last are children of the root with the given names, offsets, durations, payloads (via `spanDetail`) and status; the trace's duration covers them; `error_count` includes the marked error.
- `startMs and durationMs move the trace`; `scores replace by name`; `a marked turn sets the previews`.
- `a showcase can belong to a session`: a test showcase with `session: "sess-x"` gets that `session_id` and appears in `dataset.session("sess-x", true, true)`.

**Done when:** the phase's checks pass with `src/mocks/generated/dataset.test.ts src/mocks/generated/profiles.test.ts src/mocks/generated/stories.test.ts src/mocks/generated/handlers.test.ts`; `git status` shows only the two files.

**Commit:** `Let a profile mark chosen generated traces`

---

## Shared rules for P2-P5

- Each story module exports `<STORY>_STORY_PROFILE: Profile` (the M16 profile spread, plus `marks` and, where said, `showcases`) and `<story>Facts(dataset: Dataset): <Story>Facts` (a typed object of the numbers below). Its test builds the dataset with `createDataset({ seed: 1, now: NOW, traces, profile: <STORY>_STORY_PROFILE })`, `NOW = Date.parse("2026-11-19T12:00:00Z")`, traces 5,000 for support, 2,000 otherwise, and asserts every fact below **exactly** (or within the stated tolerance). The test also asserts the facts at another `now` (`2026-12-03T09:30:00Z`) within the tolerances, so the dev UI (real clock) shows the same story.
- Marks choose traces deterministically: sort the candidate generated traces by index and pick by rank (`rank % 100 < k` picks k%), or by the order given. Never `Math.random`. When a step needs a random value, use `mulberry32(mix(context.seed ^ SALT, index))` with a story-specific salt constant.
- "Generated traces" means non-showcase indices; "visible" means what `visibleTraces` keeps with the list's defaults (no eval, no experiments): facts count visible traces only.
- Don't change the M16 profiles in `profiles.ts` or `SUPPORT_PROFILE`; spread them.

## P2 Support story

**Edit:** new `generated/story-support.ts`, new `generated/story-support.test.ts`.

`SUPPORT_STORY_PROFILE = { ...SUPPORT_PROFILE, showcases: [...SUPPORT_PROFILE.showcases, ...SESSION_SHOWCASES], marks: supportMarks }`.

**Marks** (`C = now − 46 h`, `D = now − 5 h`):
1. **Release:** every generated trace that starts at or after `C` gets `release: "v2.14"`.
2. **Feedback:** every generated trace with `index % 3 === 0` gets a `user_feedback` score (source `end_user`, boolean `true`), except the downvotes:
   - after `C`: the first 66 such traces by index get `false`; of these, the first 41 by index also get the tag `old-pricing` and a marked span `{ name: "retrieve docs", kind: "retriever", fromMs: 300, durationMs: 180, output: json { documents: [ { id: "pricing-2025.md", score: 0.91, text: "Pro plan: $49 per seat per month (2025 pricing)." }, { id: "plans.md", score: 0.74, text: "Compare the Starter, Pro and Team plans." } ] } }`;
   - before `C`: `round(F_before × (66 / F_after) / 2)` of them get `false` (the first by index), where `F_before` and `F_after` are the counts of feedback traces before and after `C`.
3. **SSO topic:** a new turn `SSO_TURN` (question "Do you have SSO on the Starter plan?", heading "SSO on the Starter plan", summary "SSO comes with the Team plan. Starter has email and password, and Google sign-in.", steps ["Upgrade to Team to turn on SAML or OIDC SSO", "Google sign-in works on every plan"], followUps [ { question: "How much is Team?", summary: "Team is $99 per month for 5 seats.", steps: [] }, { question: "Can I try it first?", summary: "Yes, Team has a 14-day trial.", steps: [] } ], no snippets, docs ["SSO (SAML and OIDC) is part of the Team plan.", "Starter: email and password, Google sign-in.", "Team trial: 14 days."]). Generated traces **without a session**: in `[now − 7 d, now)` the first 214 by index get `turn: SSO_TURN`; in `[now − 14 d, now − 7 d)` the first 30 by index.
4. **Correctness dip:** every generated trace that already has a `correctness` score gets `correctness` = `0.82 + (index % 4) × 0.04` before `D`, and `0.42 + (index % 4) × 0.06` from `D` on (numeric, source `llm_judge`).
5. **The conversation:** `SESSION_SHOWCASES`, four showcases at indices 7, 8, 9, 10, `session: "sess-0142"`, `user: "user-031"`, service `support-bot`, tags `["email"]`, models `["claude-sonnet-4-5"]`, ages 1,500,000 / 1,380,000 / 1,200,000 / 1,080,000 ms (oldest first), each with `story.inputPreview`/`outputPreview` and spans as listed (root `support-agent` agent span, then children; offsets in ms, tokens and cost on the LLM span):

| # | Input | Output | Spans under the root | Scores |
|---|---|---|---|---|
| 1 | How can I export my traces to a CSV file? | Open Traces, filter the rows you want, then use Export → CSV. Exports run in the background and you get a link by email. | `retrieve docs` retriever 120-420; `answer` llm 450-3,900 (in 1,820, out 210, $0.0086) | correctness 0.92 (llm_judge) |
| 2 | What's the limit for one export? | 100,000 traces per export. Above that, use the API with a cursor. | `answer` llm 80-2,100 (in 1,960, out 64, $0.0068) | correctness 0.95 |
| 3 | Can I schedule it every week? | You can export any time from the Traces page with Export → CSV. | `answer` llm 90-1,700 (in 2,010, out 41, $0.0066) | correctness 0.3, user_feedback false (end_user) |
| 4 | Okay, thanks. | You're welcome. | `answer` llm 60-700 (in 2,060, out 8, $0.0063) | — |

   Turn 3's answer misses the weekly schedule (a prompt-fixable miss: the instructions don't tell the model to say when a feature doesn't exist). The payloads of each span: the LLM span's input is messages (system "You are a concise support agent. Answer in Markdown." then the user question), its output the answer; the retriever's output lists `export.md` and `limits.md` documents. Use the `story` payload mechanism (`Showcase.story.payloads` by span position) as the other showcases do.

**Facts** (`SupportFacts`): `downvotesSinceTuesday` (= 66), `quotingOldPricing` (= 41), `quotingShare` (= 41/66, printed 62%), `downvoteRateBefore`, `downvoteRateAfter` (after/before between 1.8 and 2.2), `ssoThisWeek` (= 214), `ssoPreviousWeek` (= 30), `correctnessBeforeDip` (mean ≥ 0.85), `correctnessAfterDip` (mean ≤ 0.6), `sessionTurns` (the four turns of `sess-0142`: inputs as above, turn 3's `user_feedback` false and `correctness` 0.3).

**Tests:** one per fact; plus "the old-pricing traces are the ones a filter finds": `visibleTraces` filtered by `release = 'v2.14'` and tag `old-pricing` returns exactly the 41.

**Commit:** `Give the support project its story data`

## P3 Coding story

**Edit:** new `generated/story-coding.ts`, new `generated/story-coding.test.ts`.

`CODING_STORY_PROFILE = { ...CODING_PROFILE, marks: codingMarks }`. Windows: `Y = now − 14 h` (the cost step), `M = now − 84 h` (Monday).

**Marks:**
1. **Retries:** generated traces starting at or after `Y`, by rank: `rank % 50 < 9` (18%) get `costFactor: 12.1`, tag `retry`, `tests_pass` false (source `check`), and 6 + (index % 4) marked spans `{ name: "run_tests", kind: "tool", fromMs: 2_000 + k × 31_000, durationMs: 30_000, input: json { path: "packages/api", command: "pytest packages/api" }, output: text "timed out after 30 s", error: { message: "timed out after 30 s", stacktrace: "TimeoutError: pytest packages/api timed out after 30 s" } }` (k from 0). Only the last `run_tests` span is not an error when `index % 5 === 0` (some runs pass in the end).
2. **Context window:** generated traces starting at or after `M`, by rank `rank % 100 < 9` (9%), not already marked by 1: `error: { count: 1, spanName: "write patch", message: "ContextWindowExceeded: diff over 23 files", stacktrace: "ContextWindowExceeded: the prompt is 212,431 tokens; the limit is 200,000" }`, tag `context-window`. No trace before `M` has it.

**Facts** (`CodingFacts`): `retryShare` (marked runs / runs since `Y` = 0.18 ± 0.005), `retryCostShare` (their cost / all cost since `Y` = 0.73 ± 0.02), `costPerRunRatio` (mean cost per run since `Y` / mean cost per run in `[Y − 24 h, Y)` = 3.0 ± 0.2), `hourlyCostSinceStep` (minimum hourly `sum(cost_total)` since `Y`, > 40) and `hourlyCostBefore` (maximum hourly sum in `[now − 24 h, Y)`, < 40), `contextWindowShare` (since `M` = 0.09 ± 0.005), `contextWindowBeforeMonday` (= 0), `testsPassRateBefore` / `testsPassRateSince` (hourly mean of `tests_pass` before and since `Y`: the second lower).

**Tests:** one per fact; plus the retry runs' `run_tests` spans read `packages/api` and "timed out after 30 s" (`spanDetail`).

**Commit:** `Give the coding project its story data`

## P4 Voice story

**Edit:** new `generated/story-voice.ts`, new `generated/story-voice.test.ts`.

`VOICE_STORY_PROFILE = { ...VOICE_PROFILE, scores: interruptionScores, marks: voiceMarks }`, where `interruptionScores` gives every generated turn `interruption` 0.9 (numeric, source `check`); marks override it below. `R = now − 6 h`.

**Marks**, in this order (a later mark on the same trace merges into the earlier one; `durationMs` of the later wins):
1. **Quieter before the rise:** every generated turn starting before `R` gets `durationMs = floor(duration × 0.5)`, at least 400.
2. **Availability checks:** generated turns with `index % 10 < 3` (30%) get a marked span `{ name: "check_availability", kind: "tool", fromMs: 250, durationMs: d, input: json { day: "Friday", time: "morning" }, output: json { slots: ["9:30", "11:00"] } }` and `durationMs = d + 600`. The `d` values: sort these turns by `mix(seed ^ 0x51ed270b, index)`; the turn of rank r among n gets `d = q((r + 0.5) / n)`, `q` linear between (0, 400), (0.5, 1,400), (0.95, 3,200), (1, 4,000) ms, rounded.
3. **Hang-ups:** for each call (session) whose number `k` has `k % 5 < 2` (40% of calls), its last turn gets tag `hangup`. For 71% of these calls by rank (`rank % 100 < 71`), the turn before the last gets a `check_availability` span with `d` = 2,600 + (k % 12) × 100 and `durationMs = d + 600`; for the others, that turn gets `durationMs` = min(its duration, 2,400) and no slow span.
4. **Interruptions:** a call started `a` whole days ago (0-13) is interrupted when `mix(seed ^ 0x2545f491, k) % 1000 < 1000 × (0.03 + 0.06 × (13 − a) / 13)`; its middle turn gets `interruption` 0.3.

**Facts** (`VoiceFacts`): `availabilityMedianMs` (= 1,400 ± 50), `availabilityP95Ms` (= 3,200 ± 100), `hangupsAfterSlowTurn` (share of hang-up calls whose turn before the last lasted over 2.5 s = 0.71 ± 0.02), `slowTurnsWithAvailability` (share of those slow turns whose longest span is `check_availability` ≥ 0.95), `interruptedCallShare` (over 14 days = 0.06 ± 0.01), `interruptedByDay` (14 shares, the last 7 days' mean above the first 7's), `p95BeforeRise` (max hourly p95 duration before `R`, < 1,500 ms), `p95SinceRise` (min hourly p95 since `R`, > 1,500 ms).

**Tests:** one per fact (`percentile` from `dataset.ts`).

**Commit:** `Give the voice project its story data`

## P5 Extraction story

**Edit:** new `generated/story-extraction.ts`, new `generated/story-extraction.test.ts`.

`EXTRACTION_STORY_PROFILE = { ...EXTRACTION_PROFILE, marks: extractionMarks }`. `B = now − 3 h`.

**Vendors:** four comma-decimal vendors as new turns (no snippets, no docs, the same two follow-ups as the M16 extraction turns): Nordic Freight (`invoice-2026-0521.pdf (2 pages)`, summary `Nordic Freight · INV-2026-0521 · 2026-11-18 · 6 line items · 1.234,56 EUR`), Baltic Paper (`invoice-2026-0524.pdf (1 page)`, `Baltic Paper · INV-2026-0524 · 2026-11-18 · 3 line items · 812,40 EUR`), Rhine Tools (`invoice-2026-0527.pdf (3 pages)`, `Rhine Tools · INV-2026-0527 · 2026-11-18 · 9 line items · 2.976,10 EUR`), Lyon Bureau (`invoice-2026-0530.pdf (1 page)`, `Lyon Bureau · INV-2026-0530 · 2026-11-18 · 2 line items · 540,00 EUR`). A generated trace with `index % 20 < 4` gets one of them as its `turn` (index % 20 = 0 Nordic Freight, 1 Baltic Paper, 2 Rhine Tools, 3 Lyon Bureau).

**Marks:**
1. **Re-extractions:** Nordic Freight traces by rank `rank % 50 < 19` (38%), the other three comma vendors by rank `rank % 100 < 30` (30%), the rest by rank `rank % 100 < 5` (5%): tag `retry` and two marked spans `{ name: "validate schema", kind: "evaluator", fromMs: 3_100, durationMs: 40, output: json { valid: false, field: "total", reason: "expected a number, got \"<the vendor's amount>\"" } }` then `{ name: "re-extract fields", kind: "llm", fromMs: 3_200, durationMs: 2_400 }` (for the non-comma vendors the reason is "total does not match the line items").
2. **Scanned PDFs:** generated traces by rank `rank % 100 < 3`, not marked by 1: `error: { count: 1, spanName: "ocr", message: "OCR returned no text", stacktrace: "OcrError: no text on any page (scanned image, 0 characters)" }`, tag `scanned`. Plus a burst: the first 12 by index of the generated traces starting in `[now − 1 h, now)` that are not marked by 1 also get it.
3. **The batch:** the 200 generated traces of the lowest indices among those not marked by 1 or 2 and starting before `now − 1 d` get `startMs = B + rank × 18_000` (one every 18 s over an hour), tag `batch-1119`; of these, ranks 41, 97 and 158 get `error: { count: 2, spanName: "validate schema", message: "schema check failed: total", stacktrace: "SchemaError: total does not match the line items" }` and tag `retry`.

**Facts** (`ExtractionFacts`): `nordicFreightRetryShare` (= 0.38 ± 0.01), `commaVendors` (the three other vendors whose retry share ≥ 0.25), `otherVendorsRetryShare` (≤ 0.06), `scannedShare` (over 30 days = 0.03 ± 0.005, the burst included), `scannedLastHour` (≥ 12), `scannedHourlyBefore` (max per hour in `[now − 24 h, now − 1 h)`, ≤ 8), `batchSize` (= 200), `batchFailedTwice` (= 3), `batchCost` (sum of `cost_total`, for the M19 total).

**Tests:** one per fact; plus `tags @> ['batch-1119']` over `visibleTraces` returns the 200 traces, the three failures have `error_count` 2.

**Commit:** `Give the extraction project its story data`

---

## P6 Wire the story profiles

**Goal:** the dev UI builds each project from its story profile, and the new score names have definitions.

**Edit:** `web/src/main.tsx`, `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts`.

### Steps

1. `main.tsx`: the profiles map becomes `{ support: SUPPORT_STORY_PROFILE, coding: CODING_STORY_PROFILE, voice: VOICE_STORY_PROFILE, extraction: EXTRACTION_STORY_PROFILE }`, imported from the four story modules (dynamic imports as today).
2. `handlers.ts`: add to `SCORE_DEFINITIONS` (served in every project, after `tone`): `user_feedback` (boolean, direction higher, id `…000616`) and `interruption` (numeric 0..1, direction higher, id `…000617`), in the same shape as the existing definitions.
3. Test (`handlers.test.ts`): the score-definitions route lists `user_feedback` and `interruption` after `tone`.

**Done when:** the phase's checks pass with `src/mocks/generated/handlers.test.ts` and the four story test files; then by hand, with the dev UI (`just web-dev`, or the screenshot script's Chromium if the browser pane can't register the service worker): `/acme/support-bot/sessions/sess-0142` shows the four turns; `/acme/code-agent/traces?filter=tags @> ['retry']` lists retry runs; `/acme/voice-desk/traces?filter=tags @> ['hangup']` lists hang-ups; `/acme/invoice-pipeline/traces?filter=tags @> ['batch-1119']` lists 200 invoices. Report what you saw.

**Commit:** `Build the dev projects from their story profiles`

## After P6 (the coordinator)

A Sonnet code review of M17, fixes, `just check-web`, then M18's plan uses the pinned facts.
