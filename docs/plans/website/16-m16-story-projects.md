# M16 A mock project per story

The org `acme` of the web UI's large mock gets three more projects beside "Support bot": "Code agent", "Voice desk" and "Invoice pipeline". Each has its own generated traces, from a generation profile shaped like its story: long coding runs with tool calls and failing tests, short voice turns grouped in calls, cheap invoice extractions with a schema check. Each story's hand-written objects (hero trace, issue, alert rule, annotation queue, prompt, dashboard, assistant chat) move to its project, and the 21 non-support story screenshots are retaken there. The support project keeps today's generated draws.

**Prerequisites:** M15 merged on this branch (`claude/focused-screenshots`, head 768a66a or later).

**Read first:**
- `docs/website/00-design.md` § 13 (both levers, the milestones).
- `docs/mvp-decisions.md` D-W-24, D-W-25, D-W-26.
- `web/src/mocks/generated/dataset.ts`: `Showcase` (lines 60-107), the showcases (109-611), `createDataset` (652-760), `traceSummary` (783-844), `showcaseSummary` (846-884), `userTraceMetadata` (886-895), `buildDetail` and `treeSpan` (897-1108), `SupportTurn` to `spanFromSummary` (1110-1657), `kindAt` to `scoreUuid` (2051-2131).
- `web/src/mocks/generated/handlers.ts` (`largeHandlers`, line 119; `queueItemHandlers`, 102-117), `generated/widget-query.ts` (103), `sse.ts`, `generated/stories.ts`, `stories-prompts.ts`, `story-dashboards.ts`, `dashboards.ts`, `assistant.ts`, `browser.ts`, `web/src/main.tsx`.
- `fixtures/api/static/projects.json`, `fixtures/api/static/project.json`, `fixtures/api/static/projects-activity.json` (read only: never edit `fixtures/api/`).
- `website/screenshots.json` (the 28 `marketing-*` entries), `website/src/marketing/data/stories.mjs`.

## What it is, and what it is not

When it's done:
- The project switcher of `just web-dev` lists Support bot, Playground, Code agent, Voice desk and Invoice pipeline, and each story project shows its own traces, sessions, users, issues, alert rules, queues, prompts, dashboards and assistant chat.
- `createDataset` takes a profile; `SUPPORT_PROFILE` gives today's support draws, and its dataset holds the support hero only.
- The 21 coding, voice and extraction marketing screenshots show their own project, and the other screenshots are retaken.

Out of scope:
- The M17 pictures and the data patterns they count (design § 13): new objects, new pages, new captions. M17 writes them.
- Generated datasets and experiments stay support-only; the other projects fall through to the fixtures (M17 adds the invoice dataset and experiment).
- `fixtures/api/` stays as it is. The story projects are registered with the large mock only, so in `?mock=fixtures` mode (dev only) the story prompts, story dashboards and story chats are no longer listed: they belong to projects that mode doesn't have.
- The assistant's hand-picked search rows and answers stay as they are; M17 recomputes them from the data.
- Generated spans of the story projects keep some support wording (the `embedding`, `guardrail`, `evaluator` and `span` payloads) and session follow-ups cycle; no M16 screenshot shows them, and M17 adds story text where a picture does.
- A story queue requested from another project still gets the fixture queue from `queueItemHandlers` (no screenshot does that).
- `experimentHandlers()` (generated/experiments.ts, the fixture "faq" runs) stays a `:project` wildcard, registered once with the support mock: every project answers `experiments` with those runs. M17 scopes it when it adds the invoice experiment.
- The demo build runs the same `main.tsx` block, so it builds the four datasets too (about 11,000 traces, a fraction of a second).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-26 | A picture per story, and a mock project per story (design § 13) |
| D-W-25 | Story switcher; the 28 marketing shots and `stories.mjs` |
| D-W-24 | Four hero stories: support, coding, voice, extraction |
| P-1 (planner) | Projects: `code-agent` "Code agent" id `…0003`, `voice-desk` "Voice desk" id `…0004`, `invoice-pipeline` "Invoice pipeline" id `…0005` (ids `01900000-0000-7000-8000-00000000000N`, free in the mocks and fixtures). |
| P-2 (planner) | Each hero keeps its index and trace id (support 3, coding 4, voice 5, extraction 6), in its own project's dataset only. The playground's and assistant's hard-coded hero ids stay valid. |
| P-3 (planner) | The large mock scopes a project with literal route prefixes (`/api/v1/projects/acme%2F<slug>/…`). Checked in Node: MSW decodes `%2F` in both the request and a literal path, so a literal `%2F` prefix matches `/api/v1/projects/acme%2Fcode-agent/…`. To check once in the browser (§ Done when of P6). |
| P-4 (planner) | Story objects carry a `story` field (`"support" \| "coding" \| "voice" \| "extraction"`), mapped to a project by `STORY_PROJECTS`. One rule everywhere: a story object is served only under its story's project ref. Any other ref (`acme/playground`) gets none of them. `storyHandlers` and `largeHandlers` called with no project (the unscoped `:project` route, as the existing tests use) serve every object, as today. |
| P-5 (planner) | In their own projects the coding objects drop the `retry` tag condition (the project scopes them); voice and extraction keep `voice` and `billing`, which every trace of their profiles carries. |
| P-6 (planner) | Voice traces belong to calls of 4 to 12 consecutive generated traces (consecutive among the generated indices: a showcase index is skipped), one caller per call; only the last call may be shorter. Extraction traces have no session and no user. Voice spans carry no time to first token in M16 (no M17 picture reads it; design § 13 says so). |

The user's answers this plan uses: "Ok let's keep A" (a project per story, 2026-11-24 and 2026-11-25); the 24 picture choices of design § 13 (M17).

## Versions

No new dependency.

## Phases and dependency graph

| Phase | Edit | Needs |
|---|---|---|
| P1 Profiles in the generator | `generated/dataset.ts`, `generated/dataset.test.ts` | — |
| P2 Coding, voice and extraction profiles | new `generated/profiles.ts`, new `generated/profiles.test.ts` | P1 |
| P3 The story projects | new `mocks/projects.ts`, new `mocks/projects.test.ts` | — |
| P4 Story objects per project | `generated/dataset.ts` (one line), `generated/dataset.test.ts`, `generated/handlers.test.ts`, `generated/stories.ts`, `generated/stories.test.ts`, `stories-prompts.ts`, `stories-prompts.test.ts`, `story-dashboards.ts`, `story-dashboards.test.ts`, `dashboards.ts`, `dashboards.test.ts`, `assistant.ts`, `assistant.test.ts` | P1, P2, P3 |
| P5 One large mock per project | `generated/handlers.ts`, `generated/handlers.test.ts`, `generated/widget-query.ts`, `sse.ts`, `web/src/main.tsx` | P4 |
| P6 Screenshots in the story projects | `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, PNGs under `website/src/assets/screenshots/`, docs pages under `website/src/content/docs/docs/` (alts only, if a docs shot changed), `docs/plans/website/README.md` | P5 |

All paths under `web/src/mocks/` unless they start with `web/` or `website/`. P2 and P3 run in parallel (disjoint files); the rest run in order. Each phase `git add`s its new files before `git commit -- <files>`. The parallel phases run `pnpm typecheck` only after the other one has committed (or one after the other). All phases work in the session's worktree `/home/sebastien/travaux/llm-observer/.claude/worktrees/website-research-c9eb27`, on branch `claude/focused-screenshots`; a parallel phase commits only its own files (`git commit -- <files>`).

**Checks in every phase:** run only the phase's own test files with `cd web && pnpm exec vitest run <files> --maxWorkers=4`, and `pnpm exec biome check --write <files>` then without `--write`, from `web/`. Export Node first: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`. Then type-check once: `cd web && pnpm typecheck` (routes generation and `tsc -b`; vitest does not type-check). Never run `just check`, the full vitest suite or a build; the coordinator runs the full checks once after P6.

---

## P1 Profiles in the generator

**Goal:** `createDataset` takes a `Profile` that holds every table and range of the generation, and `SUPPORT_PROFILE` reproduces today's output exactly.

**Edit:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts`.

### Contract (in `dataset.ts`, exported)

```ts
/** A question with its short Markdown answer: a topic's own, or one of its follow-ups. */
type Exchange = {
  question: string;
  summary: string;
  steps: readonly string[];
};

/** A topic of a profile's traces: turn 1 is its question, later turns of a session its follow-ups. */
export type Turn = Exchange & {
  heading: string;
  /** The main answer's code block; absent: the LLM span answers with `summary` and `steps` only. */
  snippets?: { python: string; ts: string };
  /** Retrieved documents; absent: retriever and reranker spans return no documents. */
  docs?: readonly [string, string, string];
  followUps: readonly [Exchange, Exchange, ...Exchange[]];
};

export type Sessions =
  /** A share of the traces joins a random session of the pool, as support does today. */
  | { kind: "pool"; share: number; pool: number; prefix: string }
  /** Every generated trace belongs to a call: runs of `min` to `max` consecutive generated traces. */
  | { kind: "calls"; min: number; max: number; prefix: string }
  | { kind: "none" };

export type Users =
  | { kind: "pool"; share: number; pool: number; prefix: string; label: string }
  /** One user per call (needs `sessions.kind === "calls"`): call k has user `${prefix}${pad3(k % pool)}`. */
  | { kind: "per-call"; pool: number; prefix: string; label: string }
  | { kind: "none" };

export type Profile = {
  /** `service_name` of every generated span. */
  service: string;
  /** `provider` of the generated LLM spans. */
  provider: string;
  /** Trace names; a name drawn more often is listed more times. */
  names: readonly [string, ...string[]];
  models: readonly [string, ...string[]];
  /** The span kinds by span index, cycling (index 0 is always "agent"). */
  kinds: readonly [Schema<"SpanKind">, ...Schema<"SpanKind">[]];
  /** Span names by kind, picked by span index; a kind without names is named `${kind} ${index}`. */
  spanNames: Partial<Record<Schema<"SpanKind">, readonly [string, ...string[]]>>;
  /** Tags every generated trace carries, then 0 to 2 drawn from `pool`. */
  tags: { base: readonly string[]; pool: readonly [string, ...string[]] };
  spans: { min: number; span: number; maxDepth: number };
  /** Drawn duration: `min + floor(r ** skew * span)` ms. */
  duration: { min: number; span: number; skew: number };
  /** Traces with this tag rescale their duration into [min, min + span) ms, with no new draw (support's `voice`). */
  shortTag?: { tag: string; min: number; span: number };
  errorRate: number;
  /** The generated error span's status message, and its exception's stack trace. */
  error: { message: string; stacktrace: string };
  tokens: { inMin: number; inSpan: number; outMin: number; outSpan: number };
  /** USD per million tokens. */
  price: { input: number; output: number };
  sessions: Sessions;
  /** Gap between a session's turns' starts, ms. */
  turnGap: { min: number; max: number };
  users: Users;
  /** Trace 0 is a 10,000-span, 60 s trace, 30 s old (support's large-trace check). */
  bigFirstTrace: boolean;
  turns: readonly [Turn, ...Turn[]];
  /** The tool span's call: its arguments and result, from the trace index. */
  tool: (traceIndex: number) => { arguments: Record<string, unknown>; result: Record<string, unknown> };
  systemPrompt: string;
  failedAnswer: string;
  /** Scores of a generated trace. */
  scores: (index: number, errored: boolean) => Schema<"ScoreBrief">[];
  showcases: readonly Showcase[];
};

export function createDataset(options: {
  seed: number;
  now: number;
  traces: number;
  /** Default `SUPPORT_PROFILE`. */
  profile?: Profile;
}): Dataset;
```

`Showcase` becomes exported (type only), `scoreUuid` is exported, and so are the showcase constants `SUPPORT_SHOWCASE`, `CODING_SHOWCASE`, `VOICE_SHOWCASE`, `EXTRACTION_SHOWCASE`. `SupportTurn` is renamed `Turn` everywhere; `SUPPORT_TURNS` keeps its name and type `readonly [Turn, ...Turn[]]`. `SHOWCASES` stays exported, with the same four entries.

`SUPPORT_PROFILE` (exported), in full:

```ts
export const SUPPORT_PROFILE: Profile = {
  service: "support-bot",
  provider: "anthropic",
  names: NAMES,
  models: MODELS,
  kinds: KINDS,
  spanNames: {},
  tags: { base: [], pool: TAGS },
  spans: { min: 3, span: 58, maxDepth: 6 },
  duration: { min: DRAWN_MIN_MS, span: DRAWN_SPAN_MS, skew: 1 },
  shortTag: { tag: "voice", min: VOICE_MIN_MS, span: VOICE_SPAN_MS },
  errorRate: 0.08,
  error: { message: "tool failed", stacktrace: "Error: tool failed\n    at lookup (agent.ts:12:5)" },
  tokens: { inMin: 100, inSpan: 1_900, outMin: 20, outSpan: 400 },
  price: { input: 3, output: 15 },
  sessions: { kind: "pool", share: 0.5, pool: 120, prefix: "sess-" },
  turnGap: { min: TURN_GAP_MIN_MS, max: TURN_GAP_MAX_MS },
  users: { kind: "pool", share: 0.6, pool: 40, prefix: "user-", label: "User" },
  bigFirstTrace: true,
  turns: SUPPORT_TURNS,
  tool: (traceIndex) => ({
    arguments: { ticket_id: `T-${1000 + traceIndex}` },
    result: { ticket_id: `T-${1000 + traceIndex}`, status: "open", assignee: "queue-2" },
  }),
  systemPrompt: "You are a concise support agent. Answer in Markdown.",
  failedAnswer: FAILED_ANSWER,
  scores: (index) => (index % 2 === 0 ? scoresFor(index) : []),
  showcases: SHOWCASES,
};
```

Declare it after every constant it reads (after `SUPPORT_TURNS`, `SHOWCASES` and `FAILED_ANSWER`). `NAMES`, `MODELS`, `KINDS` and `TAGS` must be typed as non-empty tuples (`as const` arrays are; if `TAGS`' `as const` gives a readonly tuple of literals, widen with `satisfies readonly [string, ...string[]]`).

### Steps

1. Thread the profile. Every function that reads a constant the profile now holds takes the profile as a parameter instead: `traceSummary(seed, index, now, profile, call)`, `showcaseAt(index, profile)` (finds in `profile.showcases`), `pickTags(rng, profile)`, `kindAt(index, profile)`, `llmCount(spanCount, profile)`, `turnOf(topicIndex, turn, profile)`, `withPreviews(trace, turn, profile)`, `scoredDocs(turn, scores)` (unchanged signature; returns `[]` when `turn.docs` is absent), `spreadSessionTurns(traces, seed, now, profile)`, `buildDetail(seed, index, trace, profile)`, `layoutSpans(shape, startMs, endMs, errorCount, rng, profile)`, `treeSpan(span, spanIndex, traceIndex, trace, profile)`, and `spanFromSummary` through `SpanContext`, which gets a `profile` field. `createDataset` reads `options.profile ?? SUPPORT_PROFILE` once and passes it down. Keep every function's existing order of operations.
2. `traceSummary`, in this exact order (each rng draw where today's code draws, so the support stream is unchanged):
   1. showcase → `showcaseSummary(showcase, index, now, profile)`, no draw.
   2. `spanCount = profile.bigFirstTrace && index === 0 ? 10_000 : profile.spans.min + floor(rng() * profile.spans.span)`.
   3. `errored = rng() < profile.errorRate`; `errorCount` as today.
   4. `environmentOf(rng)`, `experiment` as today.
   5. `ageMs = ageOf(rng, index, profile)`: the `index === 0` branch (30 s, no draw) only when `profile.bigFirstTrace`; otherwise the two draws as today.
   6. `drawnMs = profile.bigFirstTrace && index === 0 ? 60_000 : profile.duration.min + floor(rng() ** profile.duration.skew * profile.duration.span)`.
   7. name, model: `profile.names[floor(rng() * profile.names.length)]`, same for `models` (keep the `?? list[0]` fallback).
   8. tokens: `profile.tokens.inMin + floor(rng() * profile.tokens.inSpan)`, then out.
   9. session: `const sessionRoll = rng()` is drawn for every profile. For `pool`: `sessionRoll < share ? `${prefix}${String(floor(rng() * pool)).padStart(4, "0")}` : undefined` (the second draw only when under the share, as today). For `calls`: `call?.session` (the precomputed call, step 4 below), no second draw. For `none`: undefined, no second draw.
   10. user: `const userRoll = rng()` for every profile. For `pool`: `userRoll < share ? `${prefix}${String(floor(rng() * pool)).padStart(3, "0")}` : undefined`. For `per-call`: `call?.user`, no second draw. For `none`: undefined.
   11. `tags = pickTags(rng, profile)`: draws exactly as today over `profile.tags.pool`; the result is the sorted unique union of `profile.tags.base` and the drawn tags.
   12. Duration: `profile.shortTag && !(profile.bigFirstTrace && index === 0) && tags.includes(profile.shortTag.tag) ? shortTag.min + floor(((drawnMs - profile.duration.min) * shortTag.span) / profile.duration.span) : drawnMs`.
   13. metadata: `userTraceMetadata(user, profile)` when a user is set.
   14. `cost_total: Math.round(inputTokens * profile.price.input + outputTokens * profile.price.output) / 1_000_000`; `llm_count: llmCount(spanCount, profile)`; `scores: profile.scores(index, errorCount > 0)`. Everything else as today.
3. `userTraceMetadata(userId, profile)`: the number is the trailing digits of the id (`/(\d+)$/`, `Number.parseInt(match[1], 10)`; no match: 0), the name is `${label} ${digits}` with the digits as written in the id (for support, `User 012` as today), plan and country as today from the number. `label` comes from `profile.users` (`kind: "none"`: "User"). `showcaseSummary` calls it with the profile too.
4. Calls (`sessions.kind === "calls"`), computed in `createDataset` before the trace loop: walk the indices `0 … traces - 1`, skipping the showcase indices of the profile. Call `k` (from 0) takes the next `size_k = min + floor(mulberry32(mix(seed ^ CALL_SALT, k))() * (max - min + 1))` generated indices. When the indices run out, the last call keeps the traces it has, even fewer than `min` (no merge). Session id `${prefix}${String(k).padStart(4, "0")}`; user, when `users.kind === "per-call"`, `${users.prefix}${String(k % users.pool).padStart(3, "0")}`. Pass `call` (`{ session, user } | undefined`) to `traceSummary`. `const CALL_SALT = 0xbb67ae85;`.
5. `spreadSessionTurns`: the gap is `profile.turnGap.min + floor(random() * (profile.turnGap.max - profile.turnGap.min + 1))` (support: today's constants).
6. `turnOf` picks from `profile.turns`; `withPreviews` uses `profile.failedAnswer`.
7. `buildDetail`: `maxDepth` is `profile.bigFirstTrace && index === 0 ? 8 : profile.spans.maxDepth`; `detachedEvery` is 2_000 only for the big first trace, else 0 (today: index 0 only).
8. `treeSpan`: the span name is `spanIndex === 0 ? trace.name : pick(profile.spanNames[kind], spanIndex) ?? `${kind} ${spanIndex}``, where `pick` is the existing cycling helper (guard the absent list); `service_name: profile.service`; LLM spans `provider: profile.provider`; error spans `status_message: profile.error.message`.
9. `spanFromSummary`: the exception event's stack trace is `profile.error.stacktrace`; the LLM system message is `profile.systemPrompt`; the LLM answer is `llmAnswer(turn, language)` only when `context.turn.main && turn.snippets`, else `shortAnswer(exchange)`; `llmAnswer` reads `turn.snippets` (guarded); the tool span's input is `{ name, arguments: profile.tool(traceIndex).arguments }` with `name = pick(profile.spanNames.tool ?? TOOL_NAMES, index)`; the output is `profile.tool(traceIndex).result`; the root's failed answer is `profile.failedAnswer`; the guardrail reason keeps `turn.heading`; the reranker input lists `(turn.docs ?? []).map(…)`. Support has no `spanNames.tool`, so it keeps `TOOL_NAMES`.
10. `SHOWCASES` and the four showcase constants: export them. No showcase value changes in P1.
11. The `Dataset` type and the rest of `createDataset` don't change.

### Tests (in `dataset.test.ts`)

Before editing `dataset.ts`, add the first test and run it against today's code, so its expected value comes from the old generator:

- `the support profile keeps today's output`: `JSON.stringify` of `createDataset({ seed: 1, now: NOW, traces: 400 }).traces` plus `traceDetail` and every `spanDetail` of traces 0, 1, 2, 3, 7 and 16 (16 is voice-tagged), hashed with `node:crypto` `createHash("sha256")`; the expected hex is written into the test from the run on the old code. Then the same with `profile: { ...SUPPORT_PROFILE, showcases: SHOWCASES }` gives the same hash (P4 changes `SUPPORT_PROFILE.showcases`, and this second half keeps passing with the same expected hash). The first half (default options) is written against the old code, where `SUPPORT_PROFILE` doesn't exist yet: add the second half after the refactor. (Trace 0's detail is large; if it takes over 5 s, use traces 1, 2, 3, 7, 16 and assert trace 0's `span_count === 10_000` only.)
- `a profile without the big first trace generates trace 0 like any other`: a test-local profile `{ ...SUPPORT_PROFILE, bigFirstTrace: false }`: trace 0 has `span_count < 61` and is not 30 s old.
- `calls group consecutive generated traces`: profile `{ ...SUPPORT_PROFILE, sessions: { kind: "calls", min: 4, max: 12, prefix: "call-" }, users: { kind: "per-call", pool: 60, prefix: "caller-", label: "Caller" }, showcases: [] }`, 300 traces: every trace has a session; each session's traces are consecutive indices; every session but the last (the one holding index 299) has 4 to 12 traces, and the last 1 to 12; all traces of a session share one `user_id`, starting with `caller-`; `user.name` metadata is `"Caller NNN"` (JSON-encoded).
- `calls skip the showcase indices`: the same profile with `showcases: [SUPPORT_SHOWCASE]` (index 3): trace 3 has no `session_id`, and the call that spans index 3 continues at index 4.
- `a profile with no sessions or users`: `sessions: { kind: "none" }, users: { kind: "none" }`: no trace has `session_id` or `user_id`.
- `base tags are on every trace`: `tags: { base: ["billing"], pool: ["batch"] }`: every generated trace's tags include `billing`, are sorted and unique.
- `the duration skew`: `duration: { min: 400, span: 3_600, skew: 3 }`, no `shortTag`, 1,000 traces: every generated duration is in [400, 4000) ms, and more than 70% are under 2,000 ms.
- `span names and service come from the profile`: `spanNames: { tool: ["run_tests"] }, service: "code-agent"`: every tool span of trace 1's detail is named `run_tests`, and every span's `service_name` is `code-agent`.
- `a turn without snippets answers in plain text`: a profile whose `turns` have no `snippets`: the first generated trace with an `llm` span (span count 4 or more) has an LLM span output with no "```".
- The existing tests stay and pass unchanged.

**Done when:** `pnpm exec vitest run src/mocks/generated/dataset.test.ts src/mocks/generated/handlers.test.ts src/mocks/generated/stories.test.ts src/mocks/story-dashboards.test.ts src/mocks/assistant.test.ts src/mocks/playground.test.ts --maxWorkers=4` passes (only `dataset.test.ts` changed; the others prove support is unchanged); `pnpm typecheck` passes; Biome clean; `git status` shows only the two files.

**Commit:** `Generate the mock dataset from a profile`

---

## P2 Coding, voice and extraction profiles

**Goal:** three profiles, each with its own showcase only, generating traffic shaped like its story.

**Edit:** new `web/src/mocks/generated/profiles.ts`, new `web/src/mocks/generated/profiles.test.ts`.

### Contract

```ts
import { CODING_SHOWCASE, EXTRACTION_SHOWCASE, type Profile, scoreUuid, VOICE_SHOWCASE } from "./dataset";
export const CODING_PROFILE: Profile;
export const VOICE_PROFILE: Profile;
export const EXTRACTION_PROFILE: Profile;
```

Each showcase's `user` must match its profile: P2 can't edit `dataset.ts`, so the profiles use `{ ...CODING_SHOWCASE, user: "dev-007" }` and `{ ...VOICE_SHOWCASE, user: "caller-019" }` in their `showcases`; extraction's showcase has no user and stays as is.

### The values (exact)

**`CODING_PROFILE`**
- `service: "code-agent"`, `provider: "anthropic"`.
- `names`: `["coding-agent", "coding-agent", "coding-agent", "coding-agent", "fix-issue", "fix-issue", "review-pr", "write-tests", "refactor", "triage-issue"]`.
- `models`: `["claude-sonnet-4-5" ×7, "claude-haiku-4-5" ×3, "gpt-4.1" ×2]` (written out, 12 entries).
- `kinds`: `["llm", "tool", "tool", "chain", "llm", "tool", "tool", "tool", "llm", "evaluator"]`.
- `spanNames`: `{ llm: ["plan", "write patch", "review patch"], tool: ["read_file", "search_code", "apply_patch", "run_tests", "git_diff"], chain: ["attempt"], evaluator: ["tests pass"] }`.
- `tags`: `{ base: [], pool: ["ci", "pr", "retry"] }`.
- `spans: { min: 20, span: 181, maxDepth: 4 }`; `duration: { min: 8_000, span: 82_000, skew: 1 }`; no `shortTag`.
- `errorRate: 0.12`; `error: { message: "1 failed, 47 passed", stacktrace: "AssertionError: expected 3 columns, got 4\n    at test_export_csv (tests/test_export.py:41)" }`.
- `tokens: { inMin: 10_000, inSpan: 290_000, outMin: 500, outSpan: 7_500 }`; `price: { input: 3, output: 15 }`.
- `sessions: { kind: "pool", share: 0.3, pool: 60, prefix: "task-" }`; `turnGap: { min: 60_000, max: 1_800_000 }`.
- `users: { kind: "pool", share: 1, pool: 12, prefix: "dev-", label: "Developer" }`.
- `bigFirstTrace: false`.
- `tool: (i) => ({ arguments: { path: "src/export.py" }, result: { ok: true, lines: 40 + (i % 60) } })`.
- `systemPrompt: "Fix the issue with the smallest patch. Run the tests before you stop."`.
- `failedAnswer: "Stopped: 1 failed, 47 passed.\n\n- The patch did not fix test_export_csv\n- Hand the issue back with the failing test"`.
- `scores: (index, errored) => index % 2 === 0 ? [{ name: "tests_pass", score_id: scoreUuid(index, 0), source: "check", value: { data_type: "boolean", value: !errored } }] : []`. `scoreUuid` is imported from `./dataset` (P1 exports it).
- `showcases: [{ ...CODING_SHOWCASE, user: "dev-007" }]`.
- `turns` (no `snippets`, no `docs`):

| question | heading | summary | steps | follow-ups (question → summary; no steps unless given) |
|---|---|---|---|---|
| Fix the CSV export: fields that contain a comma break the columns (issue #412). | CSV export quoting | Patched 2 files; 48 tests pass. | Quote fields that contain a comma or a quote in `export_csv`; Add a test with a comma inside a field | Also escape newlines inside fields. → Patched 1 file; 49 tests pass. · Run the full test suite again. → All 49 tests pass. |
| Flaky test in test_auth_refresh (issue #398). | Flaky token refresh test | Fixed the race; 120 runs pass. | Freeze the clock in `test_auth_refresh`; Wait for the refresh task before asserting | Does it still fail on CI? → It passed 120 runs in a row on CI. · Remove the retry decorator too. → Removed it; the test passes without retries. |
| Add pagination to the /traces endpoint (issue #405). | Cursor pagination | Added cursor pagination; 52 tests pass. | Add `cursor` and `limit` to `GET /traces`; Return `next_cursor` in the response | Cap the limit at 1,000. → Capped; a larger limit returns 400. · Update the API docs. → Updated `docs/api/traces.md`. |
| The nightly job crashes on empty batches (issue #423). | Empty batch crash | Stopped: 1 failed, 47 passed. | Return early when the batch is empty; `test_nightly_empty` still fails on the summary step | Look at the summary step. → Patched 1 file; 48 tests pass. (steps: Skip the summary when there are no rows) · Add a log line for empty batches. → Added `nightly: empty batch, skipped`. |

**`VOICE_PROFILE`**
- `service: "voice-agent"`, `provider: "openai"`.
- `names`: `["voice-agent" ×7, "booking-agent", "faq-agent", "handoff"]` (10 entries).
- `models`: `["gpt-4.1-mini"]`.
- `kinds`: `["span", "span", "llm", "tool", "llm", "guardrail", "span"]`.
- `spanNames`: `{ span: ["detect end of turn", "speech to text", "text to speech"], llm: ["detect intent", "draft reply"], tool: ["check_availability", "book_slot", "cancel_booking", "get_opening_hours"], guardrail: ["safety check"] }`.
- `tags`: `{ base: ["voice"], pool: ["booking", "faq"] }`.
- `spans: { min: 5, span: 5, maxDepth: 2 }`; `duration: { min: 400, span: 3_600, skew: 3 }`; no `shortTag`.
- `errorRate: 0.03`; `error: { message: "calendar API timeout", stacktrace: "TimeoutError: calendar API timeout after 3000 ms\n    at check_availability (tools/calendar.ts:58:11)" }`.
- `tokens: { inMin: 300, inSpan: 900, outMin: 20, outSpan: 80 }`; `price: { input: 0.4, output: 1.6 }`.
- `sessions: { kind: "calls", min: 4, max: 12, prefix: "call-" }`; `turnGap: { min: 5_000, max: 15_000 }`.
- `users: { kind: "per-call", pool: 60, prefix: "caller-", label: "Caller" }`.
- `bigFirstTrace: false`.
- `tool: (i) => ({ arguments: { day: ["Monday", "Tuesday", "Friday"][i % 3] ?? "Friday" }, result: { slots: ["9:30", "11:00", "16:30"] } })`.
- `systemPrompt: "You are the front desk of Bright Smile Dental. Answer in one or two short spoken sentences."`.
- `failedAnswer: "Sorry, I can't reach the calendar right now. Let me put you through to the front desk."`.
- `scores: () => []`.
- `showcases: [{ ...VOICE_SHOWCASE, user: "caller-019" }]`.
- `turns` (steps `[]`, no snippets or docs):

Follow-ups in this order (a call cycles them):

| question | heading | summary | follow-ups |
|---|---|---|---|
| Hi, can I move my dentist appointment to Friday afternoon? | Reschedule | Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you? | 4:30 works. → Done: Friday at 4:30 pm. You'll get a text to confirm. · Is it still with Doctor Lee? → Yes, with Doctor Lee. · Do I need to bring anything? → Just your insurance card. · Thanks, bye. → You're welcome. Goodbye! |
| What time do you open on Saturday? | Opening hours | We open at 9 am on Saturday and close at 1 pm. | And on Sunday? → We're closed on Sunday. · Is there parking? → Yes, behind the building, free for patients. · Can I come without an appointment? → For emergencies, yes. Otherwise please book first. · Okay, thanks. → You're welcome. Have a good day! |
| I need to cancel my cleaning next week. | Cancel | I see your cleaning on Tuesday at 10 am. Should I cancel it? | Yes, please. → It's cancelled. Would you like to book another time? · Is there a cancellation fee? → No, there's no fee with more than 24 hours' notice. · Maybe in two weeks. → I have Tuesday the 2nd at 10 am. Should I book it? · Not now, thanks. → No problem. Goodbye! |
| Is Doctor Lee available on Monday morning? | Availability | Doctor Lee has 9:30 and 11 am free on Monday. Should I book one? | 9:30, please. → Booked: Monday at 9:30 am with Doctor Lee. · How long will it take? → About 45 minutes. · Can you text me the address? → Sure, I've sent it to your phone. · That's all, thanks. → You're welcome. See you Monday! |

**`EXTRACTION_PROFILE`**
- `service: "doc-pipeline"`, `provider: "google"`.
- `names`: `["extract-invoice" ×7, "classify-document", "extract-receipt", "validate-invoice"]` (10 entries).
- `models`: `["gemini-2.5-flash" ×9, "claude-sonnet-4-5"]` (10 entries).
- `kinds`: `["tool", "tool", "llm", "evaluator", "tool", "llm"]`.
- `spanNames`: `{ tool: ["load_pdf", "ocr", "save_invoice"], llm: ["extract fields", "re-extract fields"], evaluator: ["validate schema"] }`.
- `tags`: `{ base: ["billing"], pool: ["batch", "retry"] }`.
- `spans: { min: 6, span: 5, maxDepth: 2 }`; `duration: { min: 2_000, span: 7_000, skew: 1 }`; no `shortTag`.
- `errorRate: 0.05`; `error: { message: "schema check failed: total", stacktrace: "SchemaError: total does not match the line items\n    at validate (pipeline/validate.py:27)" }`.
- `tokens: { inMin: 1_500, inSpan: 4_500, outMin: 200, outSpan: 600 }`; `price: { input: 0.3, output: 2.5 }`.
- `sessions: { kind: "none" }`; `turnGap: { min: 30_000, max: 480_000 }` (unused); `users: { kind: "none" }`.
- `bigFirstTrace: false`.
- `tool: (i) => ({ arguments: { file: `invoice-2026-${String(400 + (i % 600)).padStart(4, "0")}.pdf` }, result: { pages: 1 + (i % 3) } })`.
- `systemPrompt: "Extract the invoice fields as JSON: vendor, invoice_number, date, line_items, total, currency."`.
- `failedAnswer: "Schema check failed twice: total does not match the line items."`.
- `scores: (index, errored) => [{ name: "schema_valid", score_id: scoreUuid(index, 0), source: "check", value: { data_type: "boolean", value: !errored } }]`.
- `showcases: [EXTRACTION_SHOWCASE]`.
- `turns` (steps `[]`, no snippets or docs; every topic has the same two follow-ups: "Extract again with the strong model." → "Same fields; the total matches." and "Validate the total." → "The total matches the line items."):

| question | heading | summary |
|---|---|---|
| invoice-2026-0418.pdf (2 pages) | Alder Office Supply | Alder Office Supply · INV-2026-0418 · 2026-11-12 · 4 line items · 1,284.00 EUR |
| invoice-2026-0421.pdf (1 page) | Brightline Logistics | Brightline Logistics · INV-2026-0421 · 2026-11-13 · 2 line items · 860.50 EUR |
| invoice-2026-0425.pdf (3 pages) | Copperleaf Foods | Copperleaf Foods · INV-2026-0425 · 2026-11-14 · 11 line items · 3,912.40 EUR |
| invoice-2026-0430.pdf (1 page) | Delta Print Works | Delta Print Works · INV-2026-0430 · 2026-11-15 · 3 line items · 412.00 EUR |
| invoice-2026-0433.pdf (2 pages) | Evergreen Cleaning | Evergreen Cleaning · INV-2026-0433 · 2026-11-16 · 5 line items · 1,050.00 EUR |
| invoice-2026-0437.pdf (1 page) | Fjord Parts | Fjord Parts · INV-2026-0437 · 2026-11-17 · 7 line items · 2,276.80 EUR |

### Tests (`profiles.test.ts`; `NOW = Date.parse("2026-11-19T12:00:00Z")`, `seed: 1`, 2,000 traces each)

- `each profile holds its own hero only`: the coding dataset's trace 4 is `coding-agent` with `user_id` `dev-007`, and traces 3, 5 and 6 are generated: their `input_preview` is not their showcase's (`SUPPORT_SHOWCASE` has no `story`: compare with `SUPPORT_TURNS[3].question` instead) and their `user_id` starts with `dev-`. Same for voice (trace 5 has `user_id` `caller-019`; traces 3, 4 and 6 have `caller-` users of their calls) and extraction (trace 6 is the showcase; traces 3, 4 and 5 have no user and a preview from the extraction topics).
- `coding runs are long and tool-heavy`: every generated duration in [8, 90) s; span counts in [20, 201); at least 40% of the spans of trace 1's detail are `tool` spans, each named one of `read_file`, `search_code`, `apply_patch`, `run_tests`, `git_diff`; error rate between 8% and 16%; every error trace's error span has `status_message` "1 failed, 47 passed".
- `coding users are developers`: every trace has a `user_id` matching `/^dev-\d{3}$/`.
- `voice turns are short and in calls`: every generated trace carries `voice`, lasts under 4 s, has 5 to 9 spans, has a `call-` session and a `caller-` user shared by its call; every call but the last has 4 to 12 traces, consecutive among the generated indices (index 5, the hero, is skipped); `sessions(false, false)` returns sessions whose turns read as the voice topics (turn 1's input preview is one of the four voice questions).
- `extraction is a batch`: no trace has a session or a user; every trace carries `billing`; every trace has a `schema_valid` score whose value is `false` exactly when the trace is an error; costs are under $0.02.
- `profiles are deterministic`: two datasets of the same profile and seed are deep-equal on `traces`.

**Done when:** `pnpm exec vitest run src/mocks/generated/profiles.test.ts --maxWorkers=4` passes; `pnpm typecheck` passes; Biome clean; `git status` shows only the two new files.

**Commit:** `Add the coding, voice and extraction mock profiles`

---

## P3 The story projects

**Goal:** the mock API lists, gets and reports activity for the three new projects, and one module maps each story to its project.

**Edit:** new `web/src/mocks/projects.ts`, new `web/src/mocks/projects.test.ts`.

### Contract (`projects.ts`)

```ts
import type { HttpHandler } from "msw";

export type StoryId = "support" | "coding" | "voice" | "extraction";

export type StoryProject = {
  story: StoryId;
  /** URL slug, as in `/acme/<slug>/traces`. */
  slug: string;
  /** The API's project ref, `acme/<slug>` (MSW hands `params.project` decoded). */
  ref: string;
  id: string;
  name: string;
};

export const STORY_PROJECTS: readonly [StoryProject, StoryProject, StoryProject, StoryProject] = [
  { story: "support", slug: "support-bot", ref: "acme/support-bot", id: "01900000-0000-7000-8000-000000000001", name: "Support bot" },
  { story: "coding", slug: "code-agent", ref: "acme/code-agent", id: "01900000-0000-7000-8000-000000000003", name: "Code agent" },
  { story: "voice", slug: "voice-desk", ref: "acme/voice-desk", id: "01900000-0000-7000-8000-000000000004", name: "Voice desk" },
  { story: "extraction", slug: "invoice-pipeline", ref: "acme/invoice-pipeline", id: "01900000-0000-7000-8000-000000000005", name: "Invoice pipeline" },
];

/** The story's project. */
export function storyProject(story: StoryId): StoryProject;
/** The project of an API ref (`acme/<slug>`), when it is a story project. */
export function projectOfRef(ref: string | undefined): StoryProject | undefined;

export const projectHandlers: HttpHandler[];
```

### Steps

1. `projectHandlers`, three handlers, each returning `undefined` (fall through to the fixtures) when it doesn't apply:
   - `GET /api/v1/orgs/:org/projects`: for org `acme` only, the fixture list (`fixtures/api/static/projects.json`, imported as the other mock modules import fixtures; find how `handlers.ts` loads `static/*.json` and do the same) plus three rows, each `{ ...supportRow, id, slug, name }` from `STORY_PROJECTS` (coding, voice, extraction, in that order, after the fixture rows); `next_cursor: null`.
   - `GET /api/v1/projects/:project`: for the three new refs (`params.project` decoded, e.g. `acme/code-agent`), `{ ...projectFixture, id, slug, name }` from `static/project.json`; otherwise `undefined` (support-bot and playground keep the fixture).
   - `GET /api/v1/orgs/:org/projects/activity`: for `acme`, the fixture (`static/projects-activity.json`) with three more entries, each `{ ...entryOf0001, project_id: <id>, traces, errors, cost_usd, buckets }`: code-agent 18 traces, 2 errors, 6.42 USD; voice-desk 64, 2, 0.19; invoice-pipeline 120, 6, 0.31. `buckets` is 0001's 24 buckets with their counts scaled so they sum to the entry's totals: bucket i gets `floor(total × w_i / W)` where w_i is 0001's bucket count and W their sum, and the remainder goes to the last bucket (traces and errors separately; any cost field likewise, rounded to 4 decimals). `last_ingest_at` stays 0001's. Use the fixture's own field names (`cost_usd` for the cost; read the file for the others).
2. `projectHandlers` is not registered here: P5 registers it with the large mock in `main.tsx`.

### Tests (`projects.test.ts`, with `server` from `@/mocks/node` and `server.use(...projectHandlers)`)

- `the project list adds the story projects`: `GET /api/v1/orgs/acme/projects` lists support-bot, playground, code-agent, voice-desk, invoice-pipeline in that order, with the new ids and names.
- `a story project resolves by ref`: `GET /api/v1/projects/acme%2Fvoice-desk` returns slug `voice-desk`, name "Voice desk", id `…0004`.
- `other projects keep the fixture`: `GET /api/v1/projects/acme%2Fsupport-bot` returns the fixture (`support-bot`).
- `activity covers the story projects`: the activity response has five entries, including `…0003`-`…0005`, and each new entry's buckets sum to its totals.
- `storyProject and projectOfRef`: `storyProject("voice").slug === "voice-desk"`; `projectOfRef("acme/invoice-pipeline")?.story === "extraction"`; `projectOfRef("acme/playground") === undefined`; `projectOfRef(undefined) === undefined`.

**Done when:** `pnpm exec vitest run src/mocks/projects.test.ts --maxWorkers=4` passes; `pnpm typecheck` passes; Biome clean; `git status` shows only the two new files.

**Commit:** `Add the story projects to the mock org`

---

## P4 Story objects per project

**Goal:** each story object belongs to its story's project, and the support dataset holds the support hero only.

**Edit:** `generated/dataset.ts` (step 1 only), `generated/dataset.test.ts`, `generated/stories.ts`, `generated/stories.test.ts`, `stories-prompts.ts`, `stories-prompts.test.ts`, `story-dashboards.ts`, `story-dashboards.test.ts`, `dashboards.ts`, `dashboards.test.ts`, `assistant.ts`, `assistant.test.ts`.

### Contract

```ts
// generated/stories.ts
export function storyHandlers(dataset: Dataset, project?: string): HttpHandler[];
// When `project` is set (a ref, `acme/<slug>`), routes use the literal prefix
// `/api/v1/projects/${encodeURIComponent(project)}` and serve only the objects whose story's project has that ref;
// when unset, the prefix is `/api/v1/projects/:project` and every object is served, as today.

// story-dashboards.ts
/** The story of each story dashboard, by dashboard id. */
export const STORY_DASHBOARD_STORIES: Readonly<Record<string, StoryId>>;
```

`StoryIssue`, `StoryRule` and `StoryQueue` (stories.ts), `StoryPrompt` (stories-prompts.ts) and the assistant's `Story` get `story: StoryId` (import the type from `../projects` or `./projects`). The assistant's `Story.name` already is the story id: rename the field `name` to `story` there.

### Steps

1. `dataset.ts`: `SUPPORT_PROFILE.showcases` becomes `[SUPPORT_SHOWCASE]`. `SHOWCASES` stays (the four, for the tests that list them).
2. `stories.ts`:
   - Each object gets its story: issues 570101 support, 570102 coding, 570103 voice, 570104 extraction; rules 570201-570204 and queues 570501-570504 in the same order.
   - `storyIssue()`'s `project_id` is `storyProject(issue.story).id` (not `PROJECT_ID`; delete `PROJECT_ID` if nothing else reads it).
   - Issue 570102's `matches` becomes `(trace) => trace.status === "error"` (P-5). Rule 570202's condition drops its `filter`. Voice and extraction keep theirs.
   - `storyHandlers(dataset, project)`: compute `prefix` as in the contract. With `project` set, serve only the issues, rules and queues of `projectOfRef(project)?.story` (an unknown ref: none), and the same for their routes: an issue's occurrences and events, a rule's events, and the project's `alert-events` list (the project's story events merged with the fixture events, as today). The score definitions stay served in every project.
3. `stories-prompts.ts`: `write-patch` is `coding`, `voice-reply` `voice`, `extract-invoice` `extraction`. Every handler reads `params.project` (decoded). The list returns the fixture prompts, then the story prompts whose story's project ref is `params.project` (P-4: none for `acme/support-bot`, which has no story prompt, and none for `acme/playground`). `storyPrompt(name, project)` returns `undefined` for a story prompt of another project, so the request falls through to the fixtures.
4. `story-dashboards.ts`: `STORY_DASHBOARD_STORIES` maps 570401 support, 570402 coding, 570403 voice, 570404 extraction. The code agent health dashboard drops its `RETRY` filter (its widgets query every trace of the project; delete `RETRY` if unused). `dashboards.ts`: the list and the one-dashboard handlers skip a story dashboard (an id in `STORY_DASHBOARD_STORIES`) unless `params.project` is its story's project ref (P-4). The fixture dashboards stay in every project. The overview handler is unchanged.
5. `assistant.ts`: rename `Story.name` to `story` (type `StoryId`). The conversation list for `params.project` is the new chat (if created), then the story conversation whose project ref is `params.project` (none for a non-story ref), then the fixture conversations only when `params.project` is `acme/support-bot`. A get, patch, delete or turn on a story conversation from another project falls through. The coding story's first-turn filter becomes `status = 'error'`.
6. Tests. Rules for every test file below: a test that checks a story object fetches it under its story's project (`` `/api/v1/projects/${encodeURIComponent(storyProject(story).ref)}` ``); a test that reads hero 4, 5 or 6 builds that story's dataset with its profile. Add to each file that needs it a local helper:

```ts
const PROFILES = { support: SUPPORT_PROFILE, coding: CODING_PROFILE, voice: VOICE_PROFILE, extraction: EXTRACTION_PROFILE };
function datasetOf(story: StoryId, traces: number): Dataset {
  return createDataset({ seed: 1, now: NOW, traces, profile: PROFILES[story] });
}
function baseOf(story: StoryId): string {
  return `${window.location.origin}/api/v1/projects/${encodeURIComponent(storyProject(story).ref)}`;
}
```

   - `generated/dataset.test.ts`: the showcase tests (they use `SHOWCASES` at lines 182, 451-600, 668-674, and the helpers `showcaseOf()` at 329 and `scenarioOf(index)` at 437-450) build each showcase's dataset with its story's profile. Add `const STORY_OF_INDEX: Record<number, StoryId> = { 3: "support", 4: "coding", 5: "voice", 6: "extraction" };` and give `scenarioOf` and `showcaseOf` the profile of `STORY_OF_INDEX[showcase.index]`. New test `the support dataset holds the support hero only`: in the default dataset trace 3 is the support showcase (`span_count` equals `SUPPORT_SHOWCASE.spans.length`, `user_id` `user-012`), and traces 4, 5 and 6 are generated: their names are in the support `NAMES` and their `input_preview` is a question or follow-up of `SUPPORT_TURNS` (a topic's `question` or one of its `followUps[].question`). The P1 hash test: delete its first half (default options); the second half (`showcases: SHOWCASES`) stays with its hash.
   - `generated/handlers.test.ts`: "applies a tags filter combined with a name filter" (lines 432-437) builds its dataset with `CODING_PROFILE` and filters `tags @> ['retry'] and name = 'coding-agent'`; the expected ids are the coding dataset's traces that are `coding-agent` and carry `retry`, computed in the test from `dataset.traces`, and they must include trace 4. Any other test of this file that reads index 4, 5 or 6 as a hero: same rule (grep `traces[4]`, `[5]`, `[6]`).
   - `generated/stories.test.ts`: each test that loops over `STORY_ISSUES`, `STORY_RULES` or `STORY_QUEUES` (lines 77-160, 162-187, 236, 281-290, 326-366, 374-381) runs each object against `datasetOf(object.story, 2_000)` with `server.use(...storyHandlers(thatDataset, storyProject(object.story).ref))` and fetches under `baseOf(object.story)`. Build the four datasets once in `beforeAll`. Also: 189-206 ("leaves out a story issue…", 3 traces, unscoped) stays unscoped; 216-235 (rules, `CODING_ISSUE` occurrences, `VOICE_RULE`, the fixture issue) fetches each object under its story's project and the fixture issue under `acme/support-bot`. Assertions stay; the progress list (374-381) and the counts (95) are recomputed by the run and updated if they move, with the old and new values in the report. The id list test (208-215) fetches each project and expects its own story's issue id plus the fixture issue.
   - `stories-prompts.test.ts`: the `get(path)` helper becomes `get(story, path)` fetching under `baseOf(story)`; every test that reads a story prompt (including the loops at 54-73, the three-prompt test at 75-98, the unknown-prompt test and the ids test) asks each prompt under its own story's project (`write-patch` coding, `voice-reply` voice, `extract-invoice` extraction), and the fixture `support-reply` under `acme/support-bot`. "lists the fixture prompts, then the three story prompts" becomes three checks: `acme/code-agent` lists `summarize, support-reply, write-patch`; `acme/voice-desk` `…, voice-reply`; `acme/invoice-pipeline` `…, extract-invoice`; plus `acme/support-bot` lists `summarize, support-reply` only. The filter test (38) runs `?q=INVOICE` in `acme/invoice-pipeline` and `?tag=voice` in `acme/voice-desk`.
   - `story-dashboards.test.ts`:
     - "the browser mock store lists the story dashboards after the fixture one" (141): per story, `baseOf(story)/dashboards` lists the story's dashboard and "Support overview" (sorted as the store sorts), and the get of that dashboard returns it.
     - "the large mock answers every widget…" and "the rate widgets match the dataset" (160-226): register `largeHandlers(datasetOf(story, 5_000))` per story (no project option: P5 adds it; the unscoped route is fine for one dataset at a time) and ask each story's widgets against its own dataset. The coding error rate is compared with all visible coding traces: `traces.filter((t) => t.error_count > 0).length / traces.length` over the traces the widget counts (the same visibility rule the test's `taggedTraces` uses today, without the tag).
     - "the tag filter reaches only tagged traces" (195-206) uses the voice dataset (`datasetOf("voice", 5_000)`) with its handlers, and `taggedTraces("voice")` over that dataset.
     - "the hero traces are counted" (229-250): for each story, the hero is `datasetOf(story, 5_000).traces[index]`; when the second widget has a filter, its predicate holds for the hero; the coding widget has none, and the test asserts `queryOf(second).filter === undefined` and that the coding hero has `error_count > 0`.
   - `dashboards.test.ts`: new `a story dashboard is listed in its project only`: seeded with `[...seedDashboards(), ...storyDashboards()]`, `acme/invoice-pipeline` lists the fixture dashboard and 570404; `acme/support-bot` the fixture and 570401; `acme/playground` the fixture only.
   - `assistant.test.ts`: the module-level `BASE` is replaced by `baseOf(story) + "/assistant"`; the helpers `createChat`, `ask` and `detail` take the story as their first parameter (the fixture and new-chat tests pass `"support"`), and every call passes its conversation's story (for example `ask(story.story, story.conversationId, "hello")` at 430). "names only traces that exist" (234) builds the id set per story from `datasetOf(story.story, 50)` (heroes 4-6 now exist only there). "lists the four story chats…" (287) becomes: `acme/support-bot` lists the support chat then the fixture chat (`[supportId, OTHER_ID]`, message count 8); each other story project lists its own chat only. New test `a story chat is not served in another project`: a GET of the voice conversation under `acme/code-agent` returns 404, and under `acme/voice-desk` 200.
   - New tests elsewhere:
     - `stories.test.ts`: `a project serves only its story's objects`: `storyHandlers(datasetOf("coding", 2_000), "acme/code-agent")` lists issue 570102, rule 570202 and queue 570502 only; `storyHandlers(dataset, "acme/playground")` lists none of the story objects; `storyHandlers(dataset)` (no project) lists all four.
     - `stories-prompts.test.ts`: `a story prompt is not served in another project`: `GET …/acme%2Fcode-agent/prompts/voice-reply` returns 404, and under `acme/voice-desk` 200.

**Done when:** `pnpm exec vitest run src/mocks/generated/dataset.test.ts src/mocks/generated/handlers.test.ts src/mocks/generated/stories.test.ts src/mocks/stories-prompts.test.ts src/mocks/story-dashboards.test.ts src/mocks/dashboards.test.ts src/mocks/assistant.test.ts src/mocks/playground.test.ts src/mocks/generated/profiles.test.ts --maxWorkers=4` passes (`playground.test.ts` is expected to pass unchanged); from the repo root, `grep -rnE 'traces\[[456]\]|heroIndex|hero\.index' web/src --include=*.test.ts --include=*.test.tsx`: every match reads from a story's profile dataset (`datasetOf(story, …)` or a dataset built with `CODING_PROFILE`, `VOICE_PROFILE` or `EXTRACTION_PROFILE`), or is the support hero (3); `pnpm typecheck` passes; Biome clean; `git status` shows only the phase's files.

**Commit:** `Serve each story's objects in its own project`

---

## P5 One large mock per project

**Goal:** the dev and demo UI serve each story project its own generated dataset.

**Edit:** `generated/handlers.ts`, `generated/handlers.test.ts`, `generated/widget-query.ts`, `sse.ts`, `web/src/main.tsx`.

### Contract

```ts
// generated/handlers.ts
export function largeHandlers(
  dataset: Dataset,
  options?: {
    /** A project ref (`acme/<slug>`): only that project's requests are answered. Unset: every project, as today. */
    project?: string;
    /** Generated datasets and experiments. Default true. */
    datasets?: boolean;
  },
): HttpHandler[];

// generated/widget-query.ts
export function widgetQueryHandler(dataset: Dataset, prefix?: string): HttpHandler;
// sse.ts
export function traceStreamHandler(source: …, prefix?: string): HttpHandler; // keep the existing first parameter
export function spanStreamHandler(source: …, prefix?: string): HttpHandler;
```

`prefix` defaults to `"/api/v1/projects/:project"`; `largeHandlers` passes `` `/api/v1/projects/${encodeURIComponent(project)}` `` when `options.project` is set.

### Steps

1. `largeHandlers`: compute `prefix` once; every route of the function, of `queueItemHandlers` (give it a `prefix` parameter) and of the factories it calls uses it; call `storyHandlers(dataset, options?.project)`. The dataset routes (`dataset-trends`, `datasets…`), the experiment routes and `...experimentHandlers()` are registered only when `options?.datasets !== false`.
2. `widget-query.ts` and `sse.ts`: replace the module's `PROJECT` in these handlers by the `prefix` parameter; the other exports of `sse.ts` (`fixtureStreamHandlers`) keep `PROJECT`.
3. `main.tsx`, the `large` block becomes:

```ts
const [{ createDataset, SUPPORT_PROFILE }, { CODING_PROFILE, VOICE_PROFILE, EXTRACTION_PROFILE }, { largeHandlers }, { STORY_PROJECTS, projectHandlers }] =
  await Promise.all([
    import("./mocks/generated/dataset"),
    import("./mocks/generated/profiles"),
    import("./mocks/generated/handlers"),
    import("./mocks/projects"),
  ]);
const profiles = { support: SUPPORT_PROFILE, coding: CODING_PROFILE, voice: VOICE_PROFILE, extraction: EXTRACTION_PROFILE };
const now = Date.now();
worker.use(...projectHandlers);
for (const project of STORY_PROJECTS) {
  const dataset = createDataset({
    seed: 1,
    now,
    traces: project.story === "support" ? 5_000 : 2_000,
    profile: profiles[project.story],
  });
  worker.use(...largeHandlers(dataset, { project: project.ref, datasets: project.story === "support" }));
}
```

The playground project, which had the support dataset, now falls through to the fixtures. `worker.use` prepends, so the project handlers end up after the per-project mocks; they share no route with them, so the order doesn't matter.

### Tests (`handlers.test.ts`)

- `a project-scoped mock answers its project only`: `server.use(...largeHandlers(codingDataset, { project: "acme/code-agent" }))`: `GET /api/v1/projects/acme%2Fcode-agent/traces` returns the coding traces (trace 4 is `coding-agent`); `GET /api/v1/projects/acme%2Fplayground/traces` gets the fixture traces.
- `two project mocks don't mix`: support and voice mocks registered together: each project's `traces/a00…05` returns its own trace (support: a generated trace; voice: the voice hero).
- `datasets: false leaves the datasets to the fixtures`: `GET …/acme%2Fcode-agent/datasets` returns the fixture list, not the generated one.
- `the widget query and the trace stream follow the prefix`: `POST …/acme%2Fvoice-desk/dashboards/query` counts the voice dataset's traces.
- The existing tests (no `options`) pass unchanged.

**Done when:** `pnpm exec vitest run src/mocks/generated/handlers.test.ts src/mocks/generated/widget-query.test.ts src/mocks/sse.test.ts --maxWorkers=4` passes; `pnpm typecheck` passes; Biome clean; `git status` shows only the phase's files.

**Commit:** `Serve each story project its own generated data`

---

## P6 Screenshots in the story projects

**Goal:** the 21 coding, voice and extraction marketing screenshots are taken in their projects, every other screenshot is retaken (heroes 4-6 left the support project's lists), and the alts and captions still match.

**Edit:** `website/screenshots.json`, `website/src/marketing/data/stories.mjs`, PNGs under `website/src/assets/screenshots/`, the alt text of a docs page under `website/src/content/docs/docs/` only where its picture changed, `docs/plans/website/README.md`.

### Steps

1. `screenshots.json`, every `marketing-*-coding`, `-voice` and `-extraction` entry: replace `/acme/support-bot` by `/acme/code-agent`, `/acme/voice-desk` or `/acme/invoice-pipeline`. The three hero entries (`marketing-trace-view-coding`, `-voice`, `-extraction`) open the hero by URL, since generated traces now share the hero's name: path `…/traces?trace=a0000000000000000000000000000004` (`…05`, `…06` for voice and extraction), their first action (the click on the row's name) removed, the Timeline click kept. Support and docs entries don't change.
2. Retake the 21 story shots first: `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots <the 21 names>`. This is also the check of P-3 in the browser: each shot's `waitFor` text exists only in its project's data (the hero's name, a story issue title, a story rule name). If a shot times out on its `waitFor`, or a story project's picture shows support traces, stop and report.
3. Retake every other shot: `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots` (no names).
4. Look at each changed PNG (`git status`) next to its alt and caption: `stories.mjs` for the marketing shots, the docs page that embeds it for a docs shot (grep the PNG's name under `website/src/content/docs/`). Correct an alt or caption only where the picture no longer matches it (a count, a value, a name). The hero `label`s become `"Traces · Code agent"`, `"Traces · Voice desk"`, `"Traces · Invoice pipeline"`.
5. `docs/plans/website/README.md`: the M16 row links this plan and says done.

**Done when:** `cd website && node --test src/marketing/data/stories.test.mjs` passes; `just check-docs` passes; each changed PNG was looked at, and the report lists the alts and captions changed; `git status` shows only the phase's files.

**Commit:** `Take the coding, voice and extraction shots in their projects`

---

## After P6 (the coordinator)

Full checks once: `just check-web` and `just check-docs`. A Sonnet code review of the milestone, then a fix phase if needed.
