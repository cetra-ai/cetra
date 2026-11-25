# Session page: readable turns, navigation, and each turn's steps (D-C-28)

The session page (`/:org/:project/sessions/:id`) shows a session as a chat transcript. This plan makes the transcript easier to read, adds ways to move around a long session, and shows what the agent did in each turn (its model and tool calls) without opening the trace. It is web only: the steps come from the existing `GET /api/v1/projects/{project}/spans` list.

Implementers: the `glm-5-3` agent (Haiku when GLM has no credit), one phase at a time. Read the whole of your phase before editing.

## Prerequisites

- Worktree `/home/sebastien/travaux/llm-observer/.claude/worktrees/session-page`, branch `claude/session-page`, cut from `origin/main` at `cc7a755`.

## Read first

- `web/src/components/session/session-conversation.tsx` (all of it): the turn list, `Turn`, the bubbles, `bubbleOf`, `rawText`, `toolCallCount`.
- `web/src/components/session/session-view.tsx` (all of it): the page, `SessionHeader`, `showFirstError`.
- `web/src/components/annotation-queues/reviewer-target.tsx` lines 25–90 and 175–190: the reviewer's session view.
- `web/src/routes/$org/$project/sessions/$sessionId.test.tsx` (all of it, including `serveErrorSession`): the page tests. Tests run on the `fixtures/api` manifest mocks (`web/src/mocks/node.ts`), whose `list_spans` entry answers `fixtures/api/responses/spans.json` for any query.
- `web/src/api/queries.ts` `spanListOptions` (lines 100–150) and `web/src/api/client.ts` (`api`).
- `web/src/lib/org-home.ts` lines 1–63 and `web/src/lib/org-home.test.ts`: the localStorage-backed view store to copy for the density store, and its tests.
- `web/src/components/home/view-switch.tsx`: the radiogroup switch to copy for the density switch.
- `web/src/components/facets/preset-chips.tsx`: the `Toggle` chips to copy for the filter chips.
- `web/src/lib/shortcuts.ts` (`SHORTCUTS`, `matchesBinding`, `isTypingTarget`); `web/src/components/data-table/data-table.tsx` lines 385–415 (`onKeyDown`).
- `web/src/components/kind-icon.tsx` (`KindIcon`; its `aria-label` is the kind label, e.g. "Tool").
- `web/src/test/keyboard.test.tsx`: "tabs through session with a focus ring".
- `web/src/mocks/generated/handlers.ts` lines 1030–1170 (`userIdOfFilter`, `readQuery`, `filterTraces`, `filterSpans`, `pageSpans`) and line 534 (`listSpans`).
- `docs/spec/11-ui.md` § Sessions (the paragraph that starts "List (session, first message") and the keyboard line (`j/k move in lists`).

## What it is, and what it is not

When it's done, on the session page and in the annotation reviewer's session view:

- **Polish (A):** the conversation is at most 760 px wide; each turn's meta line reads name, time, the gap since the previous turn, scores, then the duration and cost on the right, dimmed; a failure shows once (red inset bar and "Failed" pill; no status dots); the turn menu appears on hover or focus.
- **Navigation (B):** a strip of cells, one per turn, sized by duration and coloured by status, above the conversation; clicking a cell selects that turn and scrolls to it. `j`/`k` (and ↓/↑) in the conversation select the next and previous turn. The header's errors chip selects the next failed turn after the selected one, wrapping, instead of always the first.
- **Steps (C):**
  - each turn shows its tool, retriever and guardrail calls as chips (name and duration; a failed one is red with its error message);
  - a chevron expands the turn's steps inline: the model, tool, retriever and guardrail calls in start order, with kind icon, name, model, tokens, duration, the error message when failed, and one line of the call's input preview; clicking a step opens it in the trace pane;
  - a toolbar with a search box (turn name and bubble text), filter chips with counts (Failed, Scored, Uses tools; several selected = all must hold), and a Chat / Compact switch remembered per browser;
  - Compact shows one line per turn.

Out of scope:
- No server, OpenAPI or storage change.
- No full payloads in the steps: each expanded step shows one line of its `input_preview` (the span list's first 512 characters of the input); the full input stays in the trace pane.
- The website screenshots (`just docs-screenshots`) wait for the product freeze; the docs text is updated here.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-28 (new, added by phase SP5) | The session page's steps come from `GET /spans` with `trace_id IN (<the turns' trace ids>) AND span_kind IN ('llm', 'tool', 'retriever', 'guardrail')`, `sort=start_time`, `order=asc`, `limit=1000`, over `[session start, session end + 1 ms)`, with the page's eval and experiment flags. The turns go 100 per request; each request reads at most 5 pages (5000 steps), and past that the page says some steps are not shown. Filtering on trace ids, not `session_id`, finds the steps of traces whose session id is set on the root span only (spec 01: a trace's `session_id` is its first non-null span value). Root spans are not steps. Chips show the tool, retriever and guardrail steps; the expanded list shows all four kinds, each with one line of its `input_preview`. Turn filters, search and density live in the conversation (also in the reviewer); density is stored per browser in `cetra.session_density`. |
| SL-* | `docs/plans/c-web-ui/sessions-list.md`: the session list and summary fields (unchanged). |

User's answers (2026-11-24): scope A + B + C in one plan; the steps come from the existing `/spans` API (no server change); the step kinds are LLM, tool, retriever and guardrail; each expanded step shows a one-line preview of its input (the span list's `input_preview`); the steps are found by the turns' trace ids, 100 turns per request.

## Versions

No new dependency.

## Phases and dependency graph

| Phase | Edits | Depends on |
|---|---|---|
| SP1 | `web/src/lib/session-turns.ts` (new), `web/src/lib/session-turns.test.ts` (new), `web/src/components/session/use-session-steps.ts` (new), `web/src/components/session/use-session-steps.test.tsx` (new) | — |
| SP2 | `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts` | — |
| SP3 | `web/src/components/session/session-conversation.tsx`, `web/src/components/session/session-view.tsx`, `web/src/components/annotation-queues/reviewer-target.tsx`, `web/src/routes/$org/$project/sessions/$sessionId.test.tsx` | SP1 |
| SP4 | `web/src/components/session/turn-toolbar.tsx` (new), `web/src/components/session/turn-strip.tsx` (new), `web/src/components/session/session-conversation.tsx`, `web/src/components/session/session-view.tsx`, `web/src/lib/shortcuts.ts`, `web/src/routes/$org/$project/sessions/$sessionId.test.tsx` | SP3 |
| SP5 | `docs/mvp-decisions.md`, `docs/spec/11-ui.md`, `website/src/content/docs/docs/tracing/sessions.mdx`, `skills/cetra/references/sessions.md` (generated), `CHANGELOG.md` | — |

SP1 ∥ SP2 ∥ SP5, then SP3, then SP4.

Tests that render the conversation and need no edit (they now also request `/spans`, which the manifest answers): `web/src/routes/$org/$project/users/$userId.test.tsx`, `web/src/components/user/user-panel.test.tsx`, the reviewer's `…/review/$itemId.test.tsx`. `web/src/test/keyboard.test.tsx` must keep passing (every tab stop has a focus ring).

## Conventions (every phase)

- **Never guess.** If a step is unclear, contradicts the code, or can't be done as written, STOP, make no further edits, and start your final message with `QUESTION:` and what you found.
- Work only in the worktree above, on its current branch. Do not create branches or worktrees. Never stash, reset or switch branches. Don't kill processes you didn't start.
- Edit only your phase's files.
- **No builds, tests or checks** (the user's machine is loaded): no vitest, tsc, biome or `just`, except where a phase names one command. The coordinator runs the checks once after the last phase. Write the tests anyway.
- AGENTS.md rules apply: no placeholders, no compatibility shims, tests for every behaviour, no `!` non-null assertions (`noUncheckedIndexedAccess` is on: check `undefined`).
- Every new focusable control gets the `focus-ring` class (the `Button`, `Input` and `Toggle` components already have focus styles).
- Text sizes use the named scale classes already in these files (`text-caption`, `text-body`, `text-ui`, `text-title`); numbers use the `num` class as the current meta line does.
- Commit once with the phase's subject, no attribution lines: `git add <your files> && git commit -m "<subject>"`.
- Final report: files changed, tests added, deviations, anything not done.

---

## SP1 — Turn helpers and the steps hook

**Goal:** pure helpers for filtering, searching and laying out turns, the density store, and a hook that loads the turns' steps.

**Edit:** `web/src/lib/session-turns.ts` (new), `web/src/lib/session-turns.test.ts` (new), `web/src/components/session/use-session-steps.ts` (new), `web/src/components/session/use-session-steps.test.tsx` (new).

### `web/src/lib/session-turns.ts`

Contracts, in full:

```ts
import { useSyncExternalStore } from "react";
import type { Schema } from "@/api/types";
import { quoteString } from "@/lib/dsl/dsl";
import { STORAGE_PREFIX } from "@/product";

/** The span kinds that count as a turn's steps (D-C-28). */
export const STEP_KINDS = ["llm", "tool", "retriever", "guardrail"] as const satisfies readonly Schema<"SpanKind">[];

/** The step kinds shown as chips on a collapsed turn. LLM calls show only in the expanded list. */
export const CHIP_KINDS: ReadonlySet<Schema<"SpanKind">> = new Set(["tool", "retriever", "guardrail"]);

/**
 * Turns per steps request. A filter is at most 4096 characters (spec 05 § Limits, D95): each quoted
 * id with its separator takes 36, so 100 ids make a 3671-character filter.
 */
export const STEPS_CHUNK = 100;

/** Steps per page of a steps request. */
export const STEPS_PAGE = 1000;

/** Pages read per steps request before giving up on the rest. */
export const STEPS_MAX_PAGES = 5;

export type Bubble = { kind: "markdown" | "json"; text: string };

export type TurnFilter = "failed" | "scored" | "tools";

export const TURN_FILTERS: readonly { id: TurnFilter; label: string }[] = [
  { id: "failed", label: "Failed" },
  { id: "scored", label: "Scored" },
  { id: "tools", label: "Uses tools" },
];

export type TurnDensity = "chat" | "compact";

export const DEFAULT_TURN_DENSITY: TurnDensity = "chat";

/** `cetra.session_density`: one choice per browser. */
export const TURN_DENSITY_STORAGE_KEY = `${STORAGE_PREFIX}session_density`;
```

Functions (signatures in full; bodies as described):

1. **Move** `bubbleOf`, `nonEmpty` and `rawText` from `session-conversation.tsx` into this file **unchanged in behaviour**, exported as `export function bubbleOf(payload: Schema<"Payload"> | undefined, role: "user" | "assistant"): Bubble | undefined`, `function nonEmpty(bubble: Bubble): Bubble | undefined` (not exported) and `export function rawText(payload: Schema<"Payload"> | undefined): string | undefined`. Do not edit `session-conversation.tsx` in this phase (SP3 switches it to these exports and deletes its copies).
2. `export function stepsFilter(traceIds: readonly string[]): string` returns exactly `` `trace_id IN (${ids}) AND span_kind IN (${kinds})` `` where `ids` is each trace id through `quoteString`, joined by `", "`, and `kinds` is each of `STEP_KINDS` through `quoteString`, joined by `", "`. Example for one id: `trace_id IN ('a1000000000000000000000000000001') AND span_kind IN ('llm', 'tool', 'retriever', 'guardrail')`.
3. `export function chunk<T>(items: readonly T[], size: number): T[][]`: consecutive slices of `size` (the last may be shorter); `[]` for no items.
4. `export function stepsWindow(start: string, end: string): { from: string; to: string }`: `from = new Date(Date.parse(start)).toISOString()`, `to = new Date(Date.parse(end) + 1).toISOString()`. (`Date.parse` drops the sub-millisecond digits, so `from` is at or before the start and `to` is after the end.)
5. `export function groupSteps(spans: readonly Schema<"SpanSummary">[]): Map<string, Schema<"SpanSummary">[]>`: for each span, skip it when `parent_span_id === undefined` (a root is the turn itself; a root with a remote parent counts as a step, which is accepted) or when its `span_kind` is not in `STEP_KINDS`; otherwise append it to the list of its `trace_id`, unless that list already holds its `span_id` (the first copy stays). Then sort each list by `start_time` (string `localeCompare`; the server writes RFC 3339 with nine digits and `Z`), ties by `span_id`.
6. `export function chipSteps(steps: readonly Schema<"SpanSummary">[]): Schema<"SpanSummary">[]`: the steps whose kind is in `CHIP_KINDS`, in order.
7. `export function previewLine(preview: string | undefined): string | undefined`: `undefined` when `preview` is undefined; else `preview.replace(/\s+/g, " ").trim()`, and `undefined` when that is empty.
8. `export function turnText(turn: Schema<"SessionTurn">): string`: `[turn.trace.name, bubbleOf(turn.input, "user")?.text, bubbleOf(turn.output, "assistant")?.text]`, the defined ones joined by `"\n"`.
9. `export function turnMatches(turn: Schema<"SessionTurn">, steps: readonly Schema<"SpanSummary">[] | undefined, query: string, filters: ReadonlySet<TurnFilter>): boolean`:
   - the query: trimmed and lower-cased; a blank query matches; otherwise `turnText(turn).toLowerCase().includes(needle)`;
   - `failed`: `turn.trace.status === "error"`;
   - `scored`: `turn.trace.scores.length > 0`;
   - `tools`: `steps !== undefined && steps.some((step) => step.span_kind === "tool")`;
   - every selected filter and the query must hold.
10. `export function filterCounts(turns: readonly Schema<"SessionTurn">[], stepsOf: (traceId: string) => readonly Schema<"SpanSummary">[] | undefined): Record<TurnFilter, number>`: for each filter, the number of turns that pass that filter alone (blank query).
11. `export function gapNs(previous: Schema<"SessionTurn"> | undefined, turn: Schema<"SessionTurn">): number | undefined`: `undefined` when `previous` is undefined; else `(Date.parse(turn.trace.start_time) - Date.parse(previous.trace.start_time)) * 1_000_000`, and `undefined` when that is negative or not finite.
12. `export function nextFailed(turns: readonly Schema<"SessionTurn">[], selectedTraceId: string | undefined): number`: the index of the first failed turn (`trace.status === "error"`) after the selected turn's index, wrapping to the start; with no selected turn (or an unknown id) the first failed turn; `-1` when no turn failed. When the selected turn is the only failed one, it returns its own index.
13. `export function stripWeight(turn: Schema<"SessionTurn">): number`: `Math.max(1, Math.round(turn.trace.duration_ns / 1_000_000))` (the cell's `flex-grow`, in ms).
14. Density store: copy the store of `web/src/lib/org-home.ts` lines 17–63 with these names: `getTurnDensity(): TurnDensity`, `setTurnDensity(next: TurnDensity): void`, `useTurnDensity(): TurnDensity`, key `TURN_DENSITY_STORAGE_KEY`, values `"chat"` and `"compact"`, default `DEFAULT_TURN_DENSITY`, the same in-memory fallback when storage throws.

### `web/src/components/session/use-session-steps.ts`

```ts
export type SessionSteps = {
  /** "pending" until every request answered once; "error" when one failed. */
  status: "pending" | "error" | "ready";
  /** A turn's steps once ready (an empty list when it has none); undefined before, or after an error. */
  stepsOf: (traceId: string) => readonly Schema<"SpanSummary">[] | undefined;
  /** A request stopped after STEPS_MAX_PAGES pages: some steps are not shown. */
  truncated: boolean;
};

export function useSessionSteps(args: {
  org: string;
  project: string;
  session: Pick<Schema<"SessionSummary">, "start_time" | "end_time">;
  turns: readonly Schema<"SessionTurn">[];
  includeEval: boolean;
  includeExperiments: boolean;
}): SessionSteps;
```

Also export, for the hook and its tests:

```ts
/** One steps request: the spans of up to STEPS_CHUNK turns, read page by page up to STEPS_MAX_PAGES. */
export function stepsChunkQuery(
  org: string,
  project: string,
  query: { from: string; to: string; traceIds: readonly string[]; includeEval: boolean; includeExperiments: boolean },
): {
  queryKey: readonly unknown[];
  queryFn: (context: { signal?: AbortSignal }) => Promise<{ spans: Schema<"SpanSummary">[]; truncated: boolean }>;
}
```

Steps of `stepsChunkQuery`:
1. `const base = spanListOptions(org, project, { from, to, limit: STEPS_PAGE, sort: "start_time", order: "asc", filter: stepsFilter(traceIds), includeEval, includeExperiments })`.
2. `queryKey`: `[...base.queryKey, "session-steps"]`.
3. `queryFn`: start with `cursor: string | null = null` and `spans = []`; up to `STEPS_MAX_PAGES` times: `const page = await base.queryFn({ pageParam: cursor, signal })`, push `page.data`, set `cursor = page.next_cursor ?? null`, and stop when `cursor === null`. Return `{ spans, truncated: cursor !== null }`. Errors propagate (the query fails).

Steps of `useSessionSteps`:
1. `const range = stepsWindow(session.start_time, session.end_time)` (not `window`, which would shadow the global).
2. `const chunks = chunk(turns.map((turn) => turn.trace.trace_id), STEPS_CHUNK)`.
3. `const combined = useQueries({ queries: chunks.map((ids) => ({ ...stepsChunkQuery(org, project, { from: range.from, to: range.to, traceIds: ids, includeEval, includeExperiments }), staleTime: 30_000 })), combine: combineSteps })`, where `combineSteps` is a **module-level** function (so TanStack re-runs it only when the results change):
   ```ts
   function combineSteps(
     results: readonly UseQueryResult<{ spans: Schema<"SpanSummary">[]; truncated: boolean }>[],
   ): { status: SessionSteps["status"]; grouped: Map<string, Schema<"SpanSummary">[]>; truncated: boolean }
   ```
   - `status`: `"error"` when some result has `isError`; else `"pending"` when some result `isPending`; else `"ready"` (no results: `"ready"`);
   - `grouped`: when ready, `groupSteps(results.flatMap((result) => result.data?.spans ?? []))`; otherwise an empty `Map`;
   - `truncated`: some result's `data?.truncated === true`.
4. `const stepsOf = useCallback((id: string) => (combined.status === "ready" ? (combined.grouped.get(id) ?? []) : undefined), [combined.status, combined.grouped])`.
5. Return `{ status: combined.status, stepsOf, truncated: combined.truncated }`.

### Tests

`web/src/lib/session-turns.test.ts` (plain vitest, build `SessionTurn` and `SpanSummary` objects with a small local factory that fills every required field):
- `stepsFilter quotes each trace id and lists the four kinds` (exact string for two ids);
- `stepsFilter of 100 ids fits the 4096-character filter limit`;
- `chunk splits into slices of the size`;
- `stepsWindow starts at or before the start and ends 1 ms after the end` (input with nine fractional digits);
- `groupSteps drops roots and other kinds, sorts by start time, and keeps a duplicate once`;
- `chipSteps keeps tool, retriever and guardrail steps`;
- `previewLine collapses whitespace and drops a blank preview`;
- `turnMatches searches the name and both bubbles, ignoring case and spaces`;
- `turnMatches requires every selected filter` (failed + scored);
- `turnMatches never matches the tools filter while steps are unknown`;
- `filterCounts counts each filter on its own`;
- `gapNs is undefined for the first turn and for a turn that starts earlier`;
- `nextFailed wraps, starts from the selection, and returns -1 without failures` (also: only the selected turn failed → its own index; unknown id → first failed);
- `stripWeight is at least 1`;
- `the density store keeps the choice when storage throws` (as `web/src/lib/org-home.test.ts` "keeps the choice when storage throws": `vi.spyOn(Storage.prototype, "getItem")` and `"setItem"` throwing). Reset in `afterEach`: `vi.restoreAllMocks()`, then `setTurnDensity(DEFAULT_TURN_DENSITY)` and `localStorage.clear()` (the module-level fallback otherwise leaks into the next test).

`web/src/components/session/use-session-steps.test.tsx` (`renderHook` with a `QueryClientProvider`, as the hook tests in `web/src/components/home/` do; `server.use` from `@/mocks/node` to answer `GET */api/v1/projects/:project/spans`):
- `asks the turns' steps in start order with a trace id filter` (assert the request's `filter`, `sort=start_time`, `order=asc`, `limit=1000`, `from`, `to`, and that `include_eval` is absent);
- `asks 100 turns per request` (101 turns → two requests, the second with one id);
- `groups the steps by trace and leaves roots out`;
- `is pending, then ready; a turn without steps has an empty list`;
- `reports an error and no steps`;
- `follows next pages and stops after five, marking the steps truncated` (every answer has a `next_cursor`: six pages are never asked; `truncated` is true).

**Done when:** the four files exist and are committed.

**Commit:** `Add session turn helpers and the session steps hook`

---

## SP2 — Mock: the steps filter

**Goal:** the generated mock's `GET /spans` honours the exact filter `stepsFilter` writes, so the dev server shows the right steps.

**Edit:** `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts`.

Steps:
1. Next to `userIdOfFilter`, add:
   ```ts
   /**
    * The trace ids and kinds of a filter of exactly the form
    * `trace_id IN ('<id>', …) AND span_kind IN ('<kind>', …)` (the session page's steps, D-C-28), else undefined.
    */
   export function stepsOfFilter(
     filter: string | undefined,
   ): { traceIds: ReadonlySet<string>; kinds: ReadonlySet<string> } | undefined
   ```
   Match with `/^\s*trace_id\s+IN\s*\(([^)]*)\)\s+AND\s+span_kind\s+IN\s*\(([^)]*)\)\s*$/i`; for each group, split on `,`, trim each item, require each to match `/^'([^']*)'$/` (else return undefined) and take the inner text; an empty group returns undefined.
2. In `listSpans`: compute `const steps = stepsOfFilter(parsed.filter)`. When defined, restrict the traces to `steps.traceIds.has(trace.trace_id)` before sorting, and the spans to `steps.kinds.has(span.span_kind)`. Do this in both branches (the `sort !== "start_time"` branch and `pageSpans`); for `pageSpans` add an optional last parameter `kinds?: ReadonlySet<string>` that its spans are filtered with. When defined, do not send the `X-Mock-Filter-Ignored` header (the two places that test `userIdOfFilter(...) === undefined` also require `steps === undefined`).
3. Nothing changes for other filters.

Tests (`handlers.test.ts`, following its existing `/spans` tests):
- `stepsOfFilter reads the trace ids and kinds`;
- `stepsOfFilter refuses other filters` (`user_id = 'x'`, a missing `AND span_kind`, an unquoted id, an empty list);
- `the span list keeps only the listed traces' steps for the steps filter` (no span of another trace, only the listed kinds, no ignored-filter header).

**Done when:** committed.

**Commit:** `Serve the session steps filter in the generated mock`

---

## SP3 — Turns: layout, meta line, chips and steps

**Goal:** the A polish and the C chips and expandable steps, in both the session page and the reviewer.

**Edit:** `web/src/components/session/session-conversation.tsx`, `web/src/components/session/session-view.tsx`, `web/src/components/annotation-queues/reviewer-target.tsx`, `web/src/routes/$org/$project/sessions/$sessionId.test.tsx`.

### API changes

`SessionConversation` props become:

```ts
{
  turns: readonly Schema<"SessionTurn">[];
  steps: SessionSteps;
  trace: string | undefined;
  onSelectTrace: (traceId: string) => void;
  /** The trace id of the turn to scroll to, or null. */
  scrollTarget: string | null;
  onScrolled: () => void;
  onCloseTrace?: () => void;
}
```

`scrollTarget` changes from a turn index to a trace id (SP4 filters turns, so an index would point at the wrong row). In this phase `SessionConversation` passes it to `TurnList` as `scrollTo: string | null` plus `onScrolled`; `TurnList`'s effect finds that id's index in the turns it renders, calls `virtualizer.scrollToIndex(index, { align: "center" })` when found, and calls `onScrolled()` either way.

### Steps

1. Delete `bubbleOf`, `nonEmpty`, `rawText`, the local `Bubble` type and `toolCallCount` from `session-conversation.tsx`, and the `Wrench` import (SP4 adds it back for compact rows); import `bubbleOf`, `rawText`, `type Bubble`, `chipSteps`, `gapNs`, `previewLine` from `@/lib/session-turns`.
2. **Width:** the virtualized inner div gets `mx-auto max-w-[760px]` (keep `relative w-full`).
3. **Expanded state:** `TurnList` holds `const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())` keyed by trace id (so it survives virtualization), and passes `expanded={expanded.has(traceId)}` and `onToggle` to `Turn`. `TurnList` also receives `steps: SessionSteps` and `onSelectStep: (traceId: string, spanId: string) => void`, and passes `previous` (the turn before it in `turns`, or undefined) to `Turn`.
4. **Meta line** (`Turn`), left to right, all in the existing `flex items-center gap-2` row:
   1. a toggle `Button` (`variant="ghost" size="icon-xs"`, `ChevronRight` with `rotate-90` when expanded, `aria-expanded`, `aria-label` "Show steps"/"Hide steps"), rendered only when `steps.stepsOf(traceId)` has at least one step; otherwise an empty `span` with `size-6 shrink-0` keeps the alignment;
   2. the existing select button (keeps `flex-1 min-w-0`), now containing in order: `#index` (`num`), the name (`font-medium text-foreground`), `TimeText style="time"`, then, when `gapNs(previous, turn)` is defined, `+{formatDuration(gap)}` (`num`), then the score badges (unchanged), then, when failed, the "Failed" pill (unchanged). Remove `StatusDot` (and its import);
   3. **after** the select button (so it sits at the right edge): one `span` `num shrink-0 text-muted-foreground` with `{formatDuration(duration_ns)}` and, when `cost_total` is defined, ` · {formatCost(cost_total)}`;
   4. the "…" menu trigger gets `opacity-0 group-hover:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100`, and the turn's root div gets `group`.
   Remove the "N tool calls" counter.
5. **Chips:** in `TurnBubbles` (which now also receives the turn's steps), between the user bubble and the assistant bubble, when `chipSteps(steps)` is non-empty and the turn is not expanded: a `flex flex-wrap gap-1.5 pl-6` row of at most 6 chips, then `+N more` (`text-caption text-muted-foreground`) for the rest. A chip is a `span` with `inline-flex items-center gap-1 rounded-md border bg-muted/40 px-1.5 text-caption`: `KindIcon kind`, the name (`max-w-[20ch] truncate`), and `formatDuration(duration_ns)` (`num text-muted-foreground`). A step with `status === "error"` gets `border-status-error/40 text-status-error`, and after the duration its `status_message` (`max-w-[24ch] truncate`, `title` = the full message) when defined. A turn without bubbles (`input` and `output` both undefined) shows the chips after its "No root input/output" line. In details mode (`TurnPayloads`) no chips show.
6. **Expanded steps:** when expanded, in the same place as the chips (instead of them; after "No root input/output" for a turn without bubbles), a list `ul aria-label="Steps"` with `ml-6 flex flex-col border-l pl-3`; one `li` per step (all four kinds) holding a `button type="button"` with `focus-ring flex w-full flex-col items-stretch rounded-sm px-1 py-0.5 text-left text-body hover:bg-row-hover`:
   - first line, a `div` `flex items-center gap-2`: `KindIcon`, the name (`truncate`), the model when defined (`text-muted-foreground`), `{formatTokens(total_tokens)} tokens` when `total_tokens` is defined (`num text-muted-foreground`), the `status_message` in `text-status-error truncate` when the step failed, and `ml-auto` `formatDuration(duration_ns)` (`num shrink-0 text-muted-foreground`);
   - second line, when `previewLine(input_preview)` is defined: a `div` `truncate pl-6 font-mono text-caption text-muted-foreground` with that text and `title` = the same text.
   Clicking it calls `onSelectStep(traceId, span.span_id)`. With `steps.status === "pending"`: a `text-caption text-muted-foreground` line "Loading steps"; `"error"`: "Could not load the steps"; ready and empty: "No model or tool calls".
7. **Step selection:** `SessionConversation` gets `function selectStep(traceId: string, spanId: string) { setSpan({ traceId, spanId }); if (traceId !== trace) onSelectTrace(traceId); }`. (`spanId` is already read from `span` only when `span.traceId === trace`.)
8. **Session view:** `SessionView` keeps the query and the pending and error returns; it then renders `<SessionLoaded data={detail.data} search={search} sessionId={sessionId} />`, a new component in the same file that receives the `SessionDetail` and owns everything that followed. It calls `useProject()` (for `org` and `project`) and `useNavigate()` itself; `SessionView` keeps only the query and the pending and error returns (and drops the hooks it no longer uses). `SessionLoaded` holds: `const steps = useSessionSteps({ org, project, session: data.session, turns: data.turns, includeEval: search.eval === 1, includeExperiments: search.exp === 1 })` (first, before any condition), the `scrollTarget` state (now `useState<string | null>(null)`), `selectTurn`, `showFirstError` (it now sets the failed turn's trace id; SP4 replaces it), and the JSX. When `steps.truncated`, render after the turns-truncation notice a `shrink-0 border-b bg-muted px-3 py-1 text-caption` line: "Some steps are not shown: this session has more than 5 000 steps per 100 turns."
9. **Reviewer:** `TargetDetail`'s ready variant gains `session?: Schema<"SessionSummary">`, set to `session.data.session` in the session branch of `useTargetDetail`. In `ReviewerTarget`'s `kind === "session"` branch, render a new local component `ReviewerSession({ session, turns, traceId, onSelectTrace })` with `detail.session`, `detail.turns ?? []`, `traceId`, `setTraceId`. When `detail.session` is undefined, the branch renders `<p className="p-3 text-body">Could not load this session</p>` instead. `ReviewerSession` calls `useSessionSteps({ org, project, session, turns, includeEval: false, includeExperiments: false })` and renders the current `div` with `SessionConversation` (`steps`, `scrollTarget={null}`, `onScrolled={NO_SCROLL}`).

### Tests (`$sessionId.test.tsx`)

Scope every chip and step assertion to `within(screen.getByRole("region", { name: "Conversation" }))`: once a turn is selected the trace pane shows the same span names. The manifest mock answers `GET /spans` with `fixtures/api/responses/spans.json`: sess-001's first turn (trace `…0001`) has the steps `chat` (llm), `get_order_status` (tool), `chat` (llm); its second turn (`…0002`) has one `chat`.
- "shows a turn as chat bubbles, and the payload switch only in details": delete the `getByText("1 tool calls")` assertion and assert instead that the `get_order_status` chip shows. Change nothing else in the existing tests unless they fail on the removed status dot or the moved duration and cost; keep their intent.
- `shows a turn's tool calls as chips` (first turn: a chip with `get_order_status`; no chip named `chat`);
- `expands a turn's steps in start order and opens a step in the trace pane` (click "Show steps" on the first turn: the "Steps" list has three buttons whose text contents contain, in order, `chat`, `get_order_status`, `chat` — match on `textContent`, since `KindIcon`'s `aria-label` starts each accessible name; click the `get_order_status` one: the URL's `trace` is the first turn's id and the trace region shows `get_order_status` selected);
- `shows a one-line input preview under each expanded step` (`server.use` a `/spans` answer whose tool step has `input_preview: "{\n  \"order\": 1234\n}"`: the step shows `{ "order": 1234 }`; a step without `input_preview` has no second line);
- `shows the gap since the previous turn` (the second turn's meta line has a `+` duration);
- `marks a failed step in red with its error` (`server.use` a `/spans` answer with a failed tool step and a `status_message`; the chip shows the message);
- `says when the steps could not load` (`server.use` a 500 for `/spans`; no "Show steps" button and no chips);
- `says when some steps are not shown` (`server.use` a `/spans` answer that always has a `next_cursor`: the notice shows).

**Done when:** committed.

**Commit:** `Show each session turn's steps and tidy the turn layout`

---

## SP4 — Toolbar, compact mode, turn strip, keys and next error

**Goal:** the search, filters and density switch (C), the turn strip and keyboard navigation (B), and the errors chip that walks through failed turns.

**Edit:** `web/src/components/session/turn-toolbar.tsx` (new), `web/src/components/session/turn-strip.tsx` (new), `web/src/components/session/session-conversation.tsx`, `web/src/components/session/session-view.tsx`, `web/src/lib/shortcuts.ts`, `web/src/routes/$org/$project/sessions/$sessionId.test.tsx`.

### `turn-toolbar.tsx`

```ts
export function TurnToolbar(props: {
  query: string;
  onQuery: (next: string) => void;
  filters: ReadonlySet<TurnFilter>;
  onToggleFilter: (filter: TurnFilter) => void;
  counts: Record<TurnFilter, number>;
  /** False until the steps are ready: the "Uses tools" chip is hidden. */
  stepsReady: boolean;
}): JSX.Element
```

A `div` `flex flex-wrap items-center gap-2 border-b px-4 py-2` with:
1. `Input` (`aria-label="Search turns"`, placeholder "Search turns", `className="h-7 w-56 text-body"`).
2. One chip per `TURN_FILTERS` entry (skip `tools` when `!stepsReady`): a `Toggle` from `@/components/ui/toggle` with `pressed`/`onPressedChange` and the same props and classes as `FixedPreset` in `web/src/components/facets/preset-chips.tsx`, showing the label and its count (`num`).
3. `ml-auto`: the density switch, a copy of `ProjectViewSwitch` (icon-only buttons with `role="radio"`, `aria-label` = the label and a Tooltip with it; the group `role="radiogroup"`, `aria-label="Density"`) with options `chat` "Chat" (`MessagesSquare`) and `compact` "Compact" (`Rows3`, from `lucide-react`), reading `useTurnDensity` and writing `setTurnDensity`. (Not "Transcript": the trace view already has a radio of that name on the same page.)

### `turn-strip.tsx`

```ts
export function TurnStrip(props: {
  turns: readonly Schema<"SessionTurn">[];
  /** Trace ids of the turns the search and filters keep. */
  visible: ReadonlySet<string>;
  selectedTraceId: string | undefined;
  onPick: (traceId: string) => void;
}): JSX.Element | null
```

Null when `turns.length < 2`. Otherwise `div role="group" aria-label="Turns overview"` `flex h-4 gap-px px-4 py-1` (8 px cells). One `button type="button"` per turn with `tabIndex={-1}` (the keys `j`/`k` are the keyboard path; N cells must not add N tab stops): `style={{ flexGrow: stripWeight(turn), flexBasis: 0 }}`, `min-w-[3px] rounded-[1px]`, `bg-status-error` when failed else `bg-muted-foreground/30`, `ring-1 ring-primary` when selected, `opacity-30` when not in `visible`, `aria-label` = `` `#${index} ${name}, ${formatDuration(duration_ns)}${failed ? ", failed" : ""}` `` (1-based index), `title` = the same. Click: `onPick(traceId)`.

### `session-conversation.tsx`

1. State in `SessionConversation`: `query` (string), `filters` (`ReadonlySet<TurnFilter>`), `const density = useTurnDensity()`, and `pendingScroll: string | null`.
2. `visible`: the entries `{ turn, index, previous }` of `turns` (`index` is the 1-based position in the full list, `previous = turns[index - 2]`, the real previous turn, so the gap is unchanged by filters) kept when `turnMatches(turn, steps.stepsOf(turn.trace.trace_id), query, filters)`. `TurnList` takes these entries and passes `index` and `previous` to `Turn` (replacing SP3's own index and `previous`).
3. Layout of the start pane, top to bottom: `TurnToolbar`, then `TurnStrip` (`visible` = the visible ids), then the `section aria-label="Conversation"` with the `TurnList` of `visible` (or, when `visible` is empty and `turns` is not, `p` "No turns match." `px-4 py-3 text-body text-muted-foreground`). **The toolbar and the strip are siblings above the section, outside it** (tests count the buttons inside the Conversation region).
4. **Scrolling:** `SessionConversation` consumes the external `scrollTarget` in a `useEffect([scrollTarget])`: when it is not null, if that turn is not in `visible`, clear `query` and `filters`; set `pendingScroll` to it; call `onScrolled()`. `TurnList` now takes only `scrollTo={pendingScroll}` and `onScrolled={() => setPendingScroll(null)}`: its effect scrolls when the id is among its rendered turns and then calls `onScrolled`; when the id is not rendered yet (the filters are clearing) it does nothing and waits for the next render. The strip's `onPick` and the keys select the turn (`onSelectTrace`) and set `pendingScroll`.
5. **Keys:** the `section` gets `tabIndex={0}`, `focus-ring`, and `onKeyDown`: return when `isTypingTarget(event.target)`, when `!event.currentTarget.contains(event.target as Node)` (events bubbling from the portalled "Turn actions" menu), or when `(event.target as Element).closest("[role='menu']")` is not null. `matchesBinding(event, "row-next")` → `preventDefault`, select the visible turn after the selected one (the first when none is selected or the selected one is hidden; stay at the last) and scroll to it; `row-prev` likewise backwards (stay at the first).
6. **Compact:** when `density === "compact"`, `TurnList` renders `CompactTurn` rows instead of `Turn`, and its `estimateSize` returns 28 (else `TURN_ESTIMATE`); pass `density` into `TurnList` and add `useEffect(() => { virtualizer.measure(); }, [density, virtualizer]);`. A `CompactTurn` is a `div` with the same click-to-select handler and selected/failed classes as `Turn`, and `grid grid-cols-[2.5rem_9rem_minmax(0,1fr)_3.5rem_4rem] items-center gap-2 px-4 h-7 text-body border-b`: `#index` (`num text-muted-foreground`); the name in a `button type="button"` (the keyboard path, `focus-ring truncate rounded-sm text-left font-medium`, `aria-current` when selected, `onClick` selects); the text `{user bubble text} → {assistant bubble text}` (first line of each via `bubbleOf(...)?.text.split("\n")[0]`; a missing side is left out with its arrow; `truncate text-muted-foreground`); the tool count (`Wrench` `size-3` + `num`: the number of `tool` steps; empty while steps are unknown or zero); the duration (`num text-right text-muted-foreground`).
7. The reviewer gets all of this through `SessionConversation`; its call site does not change.

### `session-view.tsx`

Replace `showFirstError` with `showNextError`: `const index = nextFailed(turns, search.trace)`; when `index >= 0`, set `scrollTarget` to that turn's trace id and `selectTurn` it. The errors chip calls it (the `onShowErrors` prop keeps its name; it is undefined when no turn failed, as today).

### `shortcuts.ts`

In `SHORTCUTS`, the `j / k, ↓ / ↑` entry's `scope` becomes `"lists, tree, session turns"`. Nothing else.

### Tests (`$sessionId.test.tsx`)

- `filters turns by search text` (sess-001: type a word of the second turn's question; one turn shown, its `#2` kept);
- `filters turns by the Failed chip and shows the counts` (`serveErrorSession()`: 3 turns, 1 failed; the Failed chip reads 1; pressing it leaves the failed turn only);
- `hides the Uses tools chip until the steps load, then filters on it` (sess-001: the chip appears with count 1; pressing it leaves the first turn);
- `says when no turn matches`;
- `keeps the gap to the real previous turn while filtered` (`serveErrorSession()`: press Failed; the failed turn's `+` gap equals the one shown without the filter);
- `remembers the compact density and shows one line per turn` (click `getByRole("radio", { name: "Compact" })`: a turn line shows `→`; `localStorage.getItem("cetra.session_density") === "compact"`; reset with `setTurnDensity("chat")` and `localStorage.clear()` after the test);
- `selects a turn from the strip` (click the cell named `#2 …`: the URL's `trace` is the second turn's id);
- `moves between turns with j and k` (focus the Conversation region, press `j` twice: the second turn is selected; `k`: the first);
- `the menu's arrow keys do not move the selection` (select the first turn, open its "Turn actions" menu, press ArrowDown: the URL's `trace` is unchanged);
- `the errors chip walks through the failed turns` (`server.use` a session like `serveErrorSession`'s with two failed turns: the first click selects the first, the second click the second, the third wraps);
- `a hidden failed turn clears the search when the errors chip selects it` (`serveErrorSession()`: type a query that hides the failed turn, click the errors chip: the search box is empty and the failed turn shows, selected);
- the existing errors-chip test keeps passing (the first failed turn when nothing is selected).

**Done when:** committed.

**Commit:** `Add session turn search, filters, compact mode, strip and keys`

---

## SP5 — Docs

**Goal:** the decision, the spec and the user docs describe the new session page.

**Edit:** `docs/mvp-decisions.md`, `docs/spec/11-ui.md`, `website/src/content/docs/docs/tracing/sessions.mdx`, `skills/cetra/references/sessions.md` (generated), `CHANGELOG.md`.

Steps:
1. `docs/mvp-decisions.md`, § Track C — Web UI: after the `| D-C-27 | Org home | … |` row, add the row `| D-C-28 | Session page | **The session page shows each turn's steps and lets you search, filter and move through the turns.** … |` with, after the bold lead, the D-C-28 text of this plan's decisions table, the polish and navigation of "What it is" in one sentence each, and "Rejected: adding the steps to `GET /sessions/{id}` (a server, OpenAPI and spec change and a heavier response for every session open; the span list already filters by trace and kind); filtering the steps on `session_id` (traces whose session id is set on the root span only would show none); the full input of each step (it needs each span's payload; the one-line `input_preview` is enough to tell calls apart, and the trace pane has the rest)."
2. `docs/spec/11-ui.md` § Sessions, in the paragraph starting "List (session, first message": replace the text from "and detail: a **chat transcript**" up to "session-level scores." with a description of the detail page as built: the transcript at most 760 px wide; per turn a meta line (`#n`, name, time, gap since the previous turn, scores, the Failed pill; duration and cost on the right; the menu on hover), the user and assistant bubbles, chips for the tool, retriever and guardrail calls, and a chevron that lists the model, tool, retriever and guardrail calls with one line of each call's input preview (a click opens the span in the trace pane); the steps from `GET /spans` as D-C-28 says, with the notice when some are not shown; a toolbar (search on the name and bubbles, the Failed, Scored and Uses tools chips with counts, the Chat/Compact switch stored in `cetra.session_density`); the turn strip; `j`/`k`; the errors chip selecting the next failed turn, wrapping; the selected turn's trace beside the conversation; session-level scores. Keep the rest of the paragraph (URL params, split default, Annotate, Add to queue) unchanged. Add that the annotation reviewer's session view is the same conversation. In the keyboard line (`` `j/k` move in lists ``), make it "move in lists and between session turns".
3. `website/src/content/docs/docs/tracing/sessions.mdx`: in the section that describes the session page in the UI (find it by the screenshot's `<Image`), add a short paragraph and a bullet list in the site's style (`website/STYLE.md`): the steps under each turn (chips, the chevron with each call's input preview, a click opens the call in the trace), searching and filtering turns, Compact, the overview strip, `j`/`k`, and the errors chip. Do not change the screenshot. Do not use any term `website/scripts/check-terms.mjs` bans (read its list first).
4. Regenerate the skill reference: from the worktree root, with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`, run `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` (the skill-reference line of the `justfile`'s `docs-gen` recipe). This is the one command this phase may run. Commit the changed `skills/cetra/references/sessions.md`. If the command fails or changes other files, STOP and ask.
5. `CHANGELOG.md` § `[Unreleased]`: under `### Changed` (create it after `### Added` if missing) add: "The session page shows each turn's tool calls and, on demand, its model and tool calls in order with a preview of their input; turns can be searched, filtered (failed, scored, uses tools) and shown one per line; an overview strip, `j`/`k` and the errors chip move between turns."

**Done when:** committed.

**Commit:** `Document the session page's steps, filters and navigation`

---

## After the last phase (coordinator)

From the worktree, with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$HOME/.npm-global/bin:$PATH`: `just check-web` and `just check-docs`; fix failures with a fix phase; check the page in the browser on `just web-dev` (session `sess-001` of `acme/support-bot`, and a demo session); a Sonnet code review of the whole branch, then fixes.
