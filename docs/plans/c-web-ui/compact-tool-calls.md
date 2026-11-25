# Compact tool calls in the Transcript and Chat views

Every tool call renders today as a `ToolCallCard`: a header, then its arguments (or its response) as a full `JsonView` with the Expand all / Collapse all / Search toolbar. A tool response is a second card. So a coding transcript (read_file → run_tests fails → patch → run_tests passes) does not fit in one screen, and a voice turn's Chat tab shows one call twice as JSON viewers. This plan makes each tool call one line, `name(args) → result · duration`, with the response folded into its call's line, errors visible on the line, and a click to expand to today's viewer. It also adds a **Full width** toggle that shows the transcript without the side pane.

**Prerequisites:** none (client only: no server, API or fixture change).

**Read first:**
- `AGENTS.md` (rules, tests).
- `web/src/components/payload/tool-call-card.tsx`, `part-view.tsx`, `message-view.tsx`, `chat-view.tsx`, `payload-view.tsx` (`MessageList` at the bottom).
- `web/src/components/trace/transcript-view.tsx` (whole file), `transcript-view.test.tsx`, `trace-view.tsx` (the `spanDetail` constant and the `SplitPane` at the end of the component), `tree-toolbar.tsx`, `chat-tab.tsx`, `span-detail.tsx` (props and the `ChatTab` render), `span-detail.test.tsx` (test "links a tool call to its response").
- `web/src/lib/format.ts` (`formatDuration`, `instantNs`).
- `fixtures/api/responses/spans/a1000000000000000000000000000001/b10000000000001{2,3,4}.json`: the support-bot trace used by the tests (LLM `…12` calls `get_order_status` with id `call_1`; tool span `…13` `get_order_status` runs 350 ms, sibling of the LLM spans; LLM `…14` gets the response).
- `docs/spec/11-ui.md` § Trace view.

## What it is, and what it is not

When done:
- In the Chat tab (and the Input/Output tab's messages view, and the session page's chat, which share the same components), a `tool_call` part is one line: `get_order_status(order_id: "1234") → {status: "shipped", eta: "2026-01-02"} · 350ms`. A `tool_call_response` part whose call (same id) is shown in the same message list is not shown again: its response is the `→ result` of the call's line, and a message made only of such folded responses is not rendered at all. A response whose call is not shown is its own line, `name → result` (name from the call id, else `response`).
- The duration is the matched tool span's (rule D-C-32 below), when the view knows the trace's spans (the trace page's span detail and the Transcript); else no duration.
- A failed matched tool span shows its status message in red on the line, with an error icon, in place of the result.
- Clicking a line toggles it open: under it, the call id, then **Arguments** and **Result**, each in today's viewer (`JsonView`, or text when not JSON), and the error message.
- In the Transcript, a tool turn is one line (kind icon + the same line, using the tool span's input, output, duration and status). Clicking it selects the span (as today) and toggles it open. LLM turns render their tool calls as lines with durations.
- The Transcript toolbar has a **Full width** toggle (`aria-pressed`). On, the trace shows the transcript alone, without the summary or span-detail pane; it is stored per browser as `cetra.transcript-wide`. The transcript keeps its `section aria-label="Transcript"`, which screenshots clip.

Not in scope: hiding the duplicate tool_call in an LLM turn when the next transcript turn is the same tool span (each stays, as one line); reading call ids from span attributes (the tree has no attributes); changing the Session page's step list or the assistant panel's tool steps.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-10 | Tree / Timeline / Transcript modes of the trace view |
| D-C-29 | Media transcripts and the audio player in the Chat tab (unchanged; media parts render as before) |
| D-C-32 (new, added in phase 3) | Tool calls render as one compact line with the response folded in; durations come from a tool span matched by name, parent and time; Transcript has a Full width toggle |

**D-C-32's matching rule** (implemented in phase 1, `matchToolSpans`): the candidates for an LLM span `L` are the trace's spans with `span_kind = "tool"` whose `parent_span_id` is defined and is in `{L.span_id} ∪ {L.parent_span_id, only if L.parent_span_id is defined}`. A tool span with no parent is never a candidate (so `undefined === undefined` never matches). Group the calls of `L` by tool name, separately for its input messages and its output messages, in message then part order.
- Input calls with name `n` (calls made before `L`): the candidates named `n` with `end ≤ L.start`, sorted by start time (stable). With `k = min(#calls, #candidates)`, the last `k` candidates map to the last `k` calls, in order (right-aligned). The earlier calls get no match.
- Output calls with name `n` (calls `L` asks for): the candidates named `n` with `start ≥ L.start`, sorted by start time (stable). With `k = min(#calls, #candidates)`, the first `k` candidates map to the first `k` calls, in order (left-aligned).
- Times are compared exactly with `nsBetween(a, b)` from `lib/format.ts` (`b − a` in ns): `end ≤ L.start` is `nsBetween(end, L.start) >= 0`, `start ≥ L.start` is `nsBetween(L.start, start) >= 0`, and the ascending sort by start time is `candidates.sort((x, y) => nsBetween(y.start_time, x.start_time))` (`nsBetween(a, b)` is `b − a`, so this is `x.start − y.start`). Not `instantNs`: it rounds above 2^53.

Rejected alternatives (for the decision text): matching on the call id (the tree has no attributes, and only some SDKs record `gen_ai.tool.call.id`); matching by name alone (repeated calls of one tool in an agent loop would all show the first span's duration).

**Full width:** a toggle, not a URL param: screenshots click it (`website/scripts/screenshots.mjs` supports `actions: [{ click }]`), and the choice is a reading preference like the split ratios.

**Transcript click:** one click on a tool line both selects the span and toggles it open, so one click shows both; in Full width the detail pane is hidden and selection only highlights the block.

**Accepted limits:** toggling Full width remounts the `TranscriptView` (collapsed sections and scroll reset), like a layout switch; a line's open state lives in the line and is lost when its virtualized row scrolls out of the window; when every output message is folded, the Chat tab's "Output" divider stays with nothing under it.

## Versions

No new dependency.

## Phases

```
P1 (lib/tool-calls.ts) → P2 (tool line in Chat and Transcript, Full width) → P3 (spec, decision, changelog, docs: main session, already drafted in the worktree)
```
Sequential: P2 uses P1. P2 is one phase because the Transcript must not be left half converted between commits (its tests query the tool turn's buttons).

Machine load rule for every phase: while iterating, run only the touched test files (`pnpm --dir web exec vitest run <paths> --maxWorkers=8`) and `pnpm --dir web exec tsc --noEmit -p .`; do not run `just check-web` (the main session runs it once at the end). Run Biome on touched files: `pnpm --dir web exec biome check --write <paths>`. If RTK wrote `web/.vitest/output.json`, delete it before committing.

---

### P1: Tool-call helpers

**Goal:** pure functions that shorten values inline, link calls to responses, and match calls to tool spans.

**Edit:** create `web/src/lib/tool-calls.ts`, `web/src/lib/tool-calls.test.ts`. Nothing else.

**Steps:**

1. Contracts, in full:

```ts
import type { Schema } from "@/api/types";

/** The tool span a tool call ran as (D-C-32). */
export type ToolSpanMatch = {
  spanId: string;
  durationNs: number;
  /** The span's status message, or "Failed", when its status is `error`. */
  error?: string;
};

export type ToolLinks = {
  /** Call id → tool name, over the shown messages and their context. */
  callNames: Map<string, string>;
  /** Call id → response, for each response in the shown messages whose call is also shown. */
  responses: Map<string, unknown>;
  /** Ids of the calls in the shown messages. A response with one of these ids is folded into its call. */
  shownCalls: Set<string>;
  /** The matched tool span of a `tool_call` part, by part identity. */
  spans: Map<Schema<"Part">, ToolSpanMatch>;
};

/** Not exported: only this module uses it. */
const INLINE_MAX = 80;

/** Arguments as `key: value, …` (no outer braces), shortened to INLINE_MAX. */
export function inlineArgs(value: unknown): string;

/** A result: a string's first non-empty line unquoted, else the inline value, shortened to INLINE_MAX. */
export function inlineResult(value: unknown): string;

/**
 * Links tool calls and responses within `messages` (the shown ones). `context` messages (earlier
 * turns, not shown) only add call names. `spans` is the result of matchToolSpans, or empty.
 */
export function toolLinks(
  messages: readonly Schema<"Message">[],
  context?: readonly Schema<"Message">[],
  spans?: Map<Schema<"Part">, ToolSpanMatch>,
): ToolLinks;

/** True when the message has parts and every part is a response folded into a shown call. */
export function foldedMessage(message: Schema<"Message">, links: ToolLinks): boolean;

/** D-C-32's matching rule. `llm` is the LLM span; `spans` is the trace's tree. */
export function matchToolSpans(
  llm: Schema<"SpanSummary">,
  spans: readonly Schema<"TreeSpan">[],
  input: readonly Schema<"Message">[],
  output: readonly Schema<"Message">[],
): Map<Schema<"Part">, ToolSpanMatch>;
```

2. Inline formatting, a private `inline(value, depth)`:
   - A string that `JSON.parse`s to an object or array (not `null`) is replaced by the parsed value first (only at depth 0; nested strings are not parsed). A string that fails to parse, or parses to anything else, stays a string.
   - `string`: `JSON.stringify(firstLine(value))`, where `firstLine` returns the first line (split on `\n`) whose `trim()` is non-empty, trimmed, or `""` if none.
   - `number`, `boolean`: `String(value)`; `null`: `null`; `undefined`: `""`.
   - Array: at depth < 2, `[` + items `inline(item, depth + 1)` joined by `, ` + `]`; at depth ≥ 2, `[…]`. Empty array: `[]` at any depth.
   - Plain object: at depth < 2, entries as `key: inline(v, depth + 1)` joined by `, `, wrapped in `{` `}`; at depth ≥ 2, `{…}`. Empty object: `{}` at any depth. Keys are written as-is (no quotes).
   - Anything else (function, symbol, bigint): `String(value)`.
3. `inlineArgs(value)`: after the depth-0 JSON parse step, `undefined`, `null`, `""` and an empty plain object give `""`. A non-empty plain object gives its entries `key: inline(v, 1)` joined by `, ` (the object form without its outer braces). Anything else gives `inline(value, 0)`. Then `shorten`.
4. `inlineResult(value)`: `undefined` gives `""`. A string that does not parse to an object or array gives `firstLine(value)` unquoted. Anything else gives `inline(value, 0)`. Then `shorten`.
5. `shorten(text)`: work on code points: `const chars = Array.from(text)`; if `chars.length > INLINE_MAX`, return `chars.slice(0, INLINE_MAX - 1).join("") + "…"`; else `text`.
6. `toolLinks`: for each part of `messages`: a `tool_call` with a string, non-empty `id` adds the id to `shownCalls` and sets `callNames[id]` to `part.name` when it is a string. Then for each `tool_call` in `context` with a string, non-empty id and a string name, set `callNames[id]` only if it is not set yet. Then for each `tool_call_response` part of `messages` with a string id in `shownCalls`, set `responses[id] = part.response` unless the id is already in `responses` (the first response wins). `spans` is the given map or a new empty `Map`.
7. `foldedMessage`: `message.parts.length > 0` and every part is a `tool_call_response` with a string id in `links.shownCalls`.
8. `matchToolSpans`: as D-C-32 above. Steps: build the candidate list from `spans` (`row.span`); walk `input` messages' parts in order, collecting `tool_call` parts with a string `name` by name; the same for `output`; for each name apply the input rule and the output rule; for each matched pair set the map entry for the part to `{ spanId: span.span_id, durationNs: span.duration_ns }` plus `error: span.status_message || "Failed"` only when `span.status === "error"` (no `error` key otherwise).

**Tests** (`tool-calls.test.ts`, plain vitest). Build tree rows with a local helper:

```ts
function row(span: Partial<Schema<"SpanSummary">> & { span_id: string; name: string; start_time: string; end_time: string }): Schema<"TreeSpan"> {
  return {
    depth: 1,
    detached: false,
    span: {
      trace_id: "a1000000000000000000000000000001",
      duration_ns: 0,
      metadata: {},
      span_kind: "tool",
      status: "ok",
      tags: [],
      ...span,
    },
  };
}
```
(if another `SpanSummary` field turns out required, add it to the defaults with a neutral value and report it). Timestamps use 9-digit fractions, such as `"2026-01-01T10:00:01.950000000Z"`. Use the support-bot values: LLM `b100000000000012` (`span_kind: "llm"`, parent `b100000000000011`, `10:00:00.100000000Z`–`10:00:01.900000000Z`), tool `b100000000000013` (parent `b100000000000011`, `10:00:01.950000000Z`–`10:00:02.300000000Z`, `duration_ns: 350_000_000`), LLM `b100000000000014` (parent `b100000000000011`, starts `10:00:02.350000000Z`).
- `inlineArgs`: `{order_id: "1234"}` → `order_id: "1234"`; `{day: "Friday", time: "morning"}` → `day: "Friday", time: "morning"`; the string `'{"path":"a.py"}'` → `path: "a.py"`; `{a: {b: {c: 1}}}` → `a: {b: {…}}`; `{list: [1, 2]}` → `list: [1, 2]`; `undefined`, `null`, `{}`, `""` → `""`; the non-JSON string `"ls -la"` → `"ls -la"` (with the quotes); `{text: "x".repeat(200)}` → a string of 80 code points (`Array.from(text).length === 80`) ending with `…`.
- `inlineResult`: `{slots: ["9:30", "11:00"]}` → `{slots: ["9:30", "11:00"]}`; `"\n  def f():\n    pass"` → `def f():`; `"48 passed in 3.2s"` → `48 passed in 3.2s`; `true` → `true`; `undefined` → `""`; `"é".repeat(100)` → 80 code points ending with `…`; a string of 50 emoji (`"😀".repeat(50)`) is not shortened.
- `toolLinks`: a call and its response in `messages` → `responses.get("call_1")` is the response, `shownCalls` has the id; a response whose call is only in `context` → not in `responses`, and `callNames.get(id)` is the context call's name; a call without an id is in neither; two responses with one id → the first; a call with an id but no string name → in `shownCalls`, not in `callNames`.
- `foldedMessage`: a tool message with one folded response → true; a folded and an unfolded response → false; no parts → false.
- `matchToolSpans`:
  - support-bot: LLM `…12`'s output call `get_order_status` → `{ spanId: "b100000000000013", durationNs: 350_000_000 }` with no `error` key; LLM `…14`'s input call → the same.
  - a failed tool span (`status: "error"`, `status_message: "1 failed, 47 passed"`) gives that `error`; with no `status_message`, `"Failed"`.
  - a tool span with another parent does not match; a child of the LLM span (parent = the LLM's id) matches an output call.
  - two output calls of one name and three later candidates → the first two candidates, in start order.
  - two input calls of one name and one earlier candidate → only the second call matches.
  - an input call whose only candidate ends after `L.start` → no match.
  - an LLM span with no parent and a tool span with no parent → no match; a child of that LLM span → matches an output call.

**Done when:** `pnpm --dir web exec vitest run src/lib/tool-calls.test.ts` passes; Biome is clean on both files; `pnpm --dir web exec tsc --noEmit -p .` passes.

**Commit:** `Add tool-call inline formatting and span matching`

---

### P2: Tool calls as one line, in Chat and in the Transcript

**Goal:** replace `ToolCallCard` with a one-line `ToolCallLine`, fold responses into calls, show matched durations, make Transcript tool turns one line, and add Full width.

**Edit:**
- delete `web/src/components/payload/tool-call-card.tsx`;
- create `web/src/components/payload/tool-call-line.tsx`, `web/src/components/payload/tool-call-line.test.tsx`;
- edit `web/src/components/payload/part-view.tsx`, `message-view.tsx`, `chat-view.tsx`, `chat-view.test.tsx`, `payload-view.tsx`;
- edit `web/src/components/trace/chat-tab.tsx`, `span-detail.tsx`, `span-detail.test.tsx`, `trace-view.tsx`, `transcript-view.tsx`, `transcript-view.test.tsx`, `tree-toolbar.tsx`;
- edit `web/src/routes/$org/$project/sessions/$sessionId.test.tsx` (only the transcript test at its line ~71, step 15) and `web/src/routes/$org/$project/traces/index.test.tsx` (only the test "shows the trace summary in transcript mode until a block is picked…" at its line ~384, step 15);
- `web/src/components/trace/trace-view.test.tsx` only if an existing case breaks (report it).

**Steps:**

1. `tool-call-line.tsx`, the contract:

```tsx
export function ToolCallLine(props: {
  name: string;
  /** The call id, shown when open. */
  id?: string;
  /** Absent for a response whose call is not shown: the line is `name → result`. */
  args?: { value: unknown };
  /** Absent when no result is known. */
  result?: { value: unknown };
  durationNs?: number;
  error?: string;
  /** Before the name, such as the Transcript's kind icon. */
  icon?: ReactNode;
  /** Called on every click of the line, after the open state toggles (the Transcript selects its span). */
  onClick?: () => void;
}): JSX.Element;
```
Imports: `type ReactNode, useState` from `react`; `ChevronDown`, `ChevronRight`, `CircleX` from `lucide-react`; `IdText` from `@/components/id-text`; `JsonView` from `@/components/payload/json-view`; `formatDuration` from `@/lib/format`; `inlineArgs`, `inlineResult` from `@/lib/tool-calls`.

2. Render: `const [open, setOpen] = useState(false)`; `const resultText = result !== undefined ? inlineResult(result.value) : ""`. A `div` (`flex min-w-0 flex-col gap-1`) holds a `button type="button"` with `aria-expanded={open}`, class `flex w-full min-w-0 items-center gap-1.5 text-left text-caption`, `data-status={error !== undefined ? "error" : undefined}`, and `onClick={() => { setOpen((current) => !current); onClick?.(); }}`. Inside the button, in order:
   - `open ? <ChevronDown … /> : <ChevronRight … />` with `className="size-3 shrink-0 text-muted-foreground" aria-hidden`;
   - `{icon}`;
   - `<span className="min-w-0 truncate font-mono">` holding: `<span className="font-medium">{name}</span>`; when `args !== undefined`, `{`(${inlineArgs(args.value)})`}`; then, when `error !== undefined`: `{" → "}<span className="text-destructive"><CircleX className="inline size-3" role="img" aria-label="Failed" />{error}</span>` (no space between the icon and the error text, so the text reads `→ 1 failed…` with one space); else when `resultText !== ""`: `{` → ${resultText}`}`;
   - when `durationNs !== undefined`: `<span className="num shrink-0 text-muted-foreground">{`· ${formatDuration(durationNs)}`}</span>`, and, so the text reads `… · 350ms`, put `{" "}` right before this span (JSX drops a bare newline between elements).
   The button's `textContent` is then exactly, for example, `get_order_status(order_id: "1234") → {status: "shipped"} · 350ms` The `role="img"` on `CircleX` is required (an `svg` has no implicit `img` role for testing-library); if Biome flags it, report it instead of suppressing.
3. When `open`, a body `div` (`flex flex-col gap-1 pl-4`), in order: `<IdText value={id} kind="call" />` when `id` is set; when `args !== undefined`, `<section aria-label="Arguments" className="flex flex-col gap-1"><p className="text-caption text-muted-foreground">Arguments</p><ValueBody value={args.value} /></section>`; when `result !== undefined`, the same with `Result`; when `error !== undefined`, `<p className="text-caption text-destructive">{error}</p>`.
4. `ValueBody` is today's `ArgumentBody` from `tool-call-card.tsx`, moved unchanged with `jsonObject`. `scrollTo`, the anchor ids and the "→ response" button go away: nothing links lines to each other any more.
5. `part-view.tsx`: delete the local `ToolLinks` type; `import { type ToolLinks } from "@/lib/tool-calls"` and re-export nothing. For `tool_call`: `id` and `name` as today; `const match = links.spans.get(part)`; render (always with `args`, so a call without arguments reads `name()`) `<ToolCallLine name={name} id={id} args={{ value: part.arguments }} result={id !== undefined && links.responses.has(id) ? { value: links.responses.get(id) } : undefined} durationNs={match?.durationNs} error={match?.error} />`. For `tool_call_response`: `id` as today (a string, else `undefined`); when `id !== undefined && links.shownCalls.has(id)`, return `null`; else render `<ToolCallLine name={(id ? links.callNames.get(id) : undefined) ?? "response"} id={id} result={{ value: part.response }} />`.
6. `message-view.tsx`: import `type ToolLinks` from `@/lib/tool-calls` instead of `part-view`. No other change.
7. `chat-view.tsx`: delete `toolLinks` (moved to the lib; update its imports). `ChatView` gets a new optional prop `toolSpans?: Map<Schema<"Part">, ToolSpanMatch>` and computes `links = toolLinks(messages, [], toolSpans)`. `MessageList` filters out the messages for which `foldedMessage(message, links)` is true before mapping (keys are computed over the kept messages). `payload-view.tsx`'s `MessageList`: import `toolLinks` and `foldedMessage` from `@/lib/tool-calls` (instead of `toolLinks` from `chat-view`), and filter folded messages the same way. Check with `rg -n "toolLinks|ToolLinks|NO_LINKS" web/src` that no other user is left.
8. `chat-tab.tsx`: `import { useMemo } from "react"`; new optional prop `spans?: readonly Schema<"TreeSpan">[]`. Compute
   ```ts
   const toolSpans = useMemo(() => {
     const row = spans?.find((candidate) => candidate.span.span_id === span.span_id);
     if (!spans || !row) return undefined;
     return matchToolSpans(row.span, spans, messagesOf(span.input), messagesOf(span.output));
   }, [span, spans]);
   ```
   and pass it to `ChatView`. The `input`/`output` passed to `ChatView` stay `messagesOf(span.input)` / `messagesOf(span.output)`: they return the payload's own array, so the `Part` objects are the same ones the map is keyed by. Never clone the messages or parts on this path.
9. `span-detail.tsx`: new optional prop `spans?: readonly Schema<"TreeSpan">[]`, passed as `<ChatTab span={detail} spans={spans} />`. `trace-view.tsx`: pass `spans={loaded.spans}` to `SpanDetail` in the `spanDetail` constant.
10. `transcript-view.tsx`, the tool turn: delete `ToolTurn` and the `ToolCallCard` import. In `TurnRow`, add `data-turn={span.span_id}` on the outer row `div`. The row's children become `{span.span_kind === "tool" ? toolContent : (<>{the header button as today}{the detail ternary as today, minus its `span.span_kind === "tool"` branch}</>)}`: the Skeleton and `previous` logic stay for non-tool spans only, and a tool span never shows the Skeleton. `toolContent` is:
    ```tsx
    <ToolCallLine
      icon={<KindIcon kind={span.span_kind} />}
      name={span.name}
      durationNs={span.duration_ns}
      error={span.status === "error" ? span.status_message || "Failed" : undefined}
      args={loaded ? { value: loaded.input?.value } : undefined}
      result={loaded?.output !== undefined ? { value: loaded.output.value } : undefined}
      onClick={onSelect}
    />
    ```
    followed by `{detail.isError ? <p className="text-caption">Could not load this span</p> : null}`, both in a fragment. Before the detail loads, the line is `name · duration` (no parentheses): accepted. LLM and other turns keep the header button and body as today. The row's outer `div` (selection style, hover prefetch, click-to-select that ignores buttons) is unchanged.
11. `transcript-view.tsx`, LLM turns: delete the local `NO_LINKS` constant. `LlmTurn` gets two new props, `llm: Schema<"SpanSummary">` and `spans: readonly Schema<"TreeSpan">[]`. In it: `const inputAll = payloadMessages(detail.input)`; `input = freshInput(seen, inputAll)` and `output` as today; `const links = toolLinks([...input, ...output], seen ?? [], matchToolSpans(llm, spans, inputAll, output))`; render the `input` and `output` messages that are not `foldedMessage(message, links)`, each `MessageView` with `links={links}`. `TurnRow` gets a `spans` prop and passes `llm={span}` and `spans`; `TranscriptView` passes its `spans` prop to each `TurnRow`. (`matchToolSpans` takes `inputAll`, the full input, so input calls that are not shown still match; only the shown ones render.)
12. `tree-toolbar.tsx`: change the import to `import type { KeyboardEvent, ReactNode } from "react";`; new optional prop `end?: ReactNode` on `TreeToolbar`, rendered last inside the toolbar `div` as `{end ? <><span className="flex-1" />{end}</> : null}`.
13. `TranscriptView`: new props `wide: boolean; onWide: (on: boolean) => void`; it passes to `TreeToolbar` `end={<Button type="button" size="xs" variant={wide ? "secondary" : "ghost"} aria-pressed={wide} onClick={() => onWide(!wide)}><Maximize2 />Full width</Button>}` (`Maximize2` from `lucide-react`; accessible name `Full width`). The `section aria-label="Transcript"` stays.
14. `trace-view.tsx`: `import { STORAGE_PREFIX } from "@/product"`; module constant ``const WIDE_KEY = `${STORAGE_PREFIX}transcript-wide`;``. In the component: `const [wide, setWide] = useState(() => localStorage.getItem(WIDE_KEY) === "1")`; `function chooseWide(on: boolean) { setWide(on); if (on) localStorage.setItem(WIDE_KEY, "1"); else localStorage.removeItem(WIDE_KEY); }`. Build the `TranscriptView` element once (`const transcript = <TranscriptView … wide={wide} onWide={chooseWide} />`, same props as today plus these two). Inside the `absolute inset-0` div, render `active === "transcript" && wide ? <div className="flex h-full min-h-0 flex-col">{transcript}</div> : <SplitPane … />`, with `start={active === "transcript" ? transcript : <TreeGrid … />}`. Place the `useState` with the component's other hooks, above every early return (such as the pending and error returns near line 196).
15. Existing tests that query `getByRole("button", { name: /get_order_status/ })` now find several buttons (the LLM call line, the tool turn line, the response line). Add this helper to each test file that needs it (no shared module):
    ```ts
    /** The tool line button whose text starts with `prefix`. */
    function toolLine(container: HTMLElement, prefix: string): HTMLElement {
      const lines = within(container).getAllByRole("button").filter((button) => button.textContent?.startsWith(prefix));
      const [line] = lines;
      if (lines.length !== 1 || !line) throw new Error(`expected one line starting with ${prefix}, got ${lines.length}`);
      return line;
    }
    ```
    The Transcript's tool turn row is `transcript.querySelector('[data-turn="b100000000000013"]')`; its line is `toolLine(row, "get_order_status(")`. Before the detail loads, the tool line reads `get_order_status · 350ms` (no `(`), and `toolLine` throws, so every use is `const line = await waitFor(() => toolLine(row, "get_order_status("))`, and the row is awaited the same way (`await waitFor(() => { const row = transcript.querySelector(...); if (!(row instanceof HTMLElement)) throw new Error("no row"); return row; })`). Update:
    - `transcript-view.test.tsx` "shows the agent section and the LLM and tool turns": click the tool row's line (wait for the row with `findBy`/`waitFor`).
    - `transcript-view.test.tsx` "marks only the selected turn…": `toolBlock` is the `[data-turn="b100000000000013"]` row, `llmBlock` the `[data-turn="b100000000000012"]` row; the assertions on them stay.
    - `$sessionId.test.tsx` "returns from a picked transcript block to the trace summary": click the tool row's line found the same way.
    - `traces/index.test.tsx` "shows the trace summary in transcript mode until a block is picked…" (line ~391): the same.
    Report any other test that breaks and how it was fixed.

**Tests:**
- `tool-call-line.test.tsx` (plain `render` from `@testing-library/react` and `fireEvent`, no providers, as `chat-view.test.tsx` does):
  - closed: `render(<ToolCallLine name="get_order_status" id="call_1" args={{ value: { order_id: "1234" } }} result={{ value: { status: "shipped" } }} durationNs={350_000_000} />)`; the button's `textContent` is exactly `get_order_status(order_id: "1234") → {status: "shipped"} · 350ms`; `aria-expanded="false"`; no `Arguments` region.
  - click opens: `aria-expanded="true"`, regions `Arguments` and `Result` exist, `call_1` is shown; a second click closes.
  - error (`error="1 failed, 47 passed"`): the button text contains `1 failed, 47 passed` and not `shipped`; `getByRole("img", { name: "Failed" })` exists; `data-status="error"`; open shows the error paragraph and the `Result` region.
  - `onClick` is called once per click.
  - no `args`, `name="response"`, result `{ ok: true }`: the text is `response → {ok: true}`; open shows no `Arguments` region.
  - `name="run" args={{ value: { a: 1 } }} result={{ value: "" }}`: the text is exactly `run(a: 1)`.
- `chat-view.test.tsx`, new cases (no `toolDefinitions` passed):
  - a call `lookup` `{q: "1"}` with id `call_1` in an assistant message and its response `{answer: 2}` in a `tool` message, all in `input` → exactly one button whose text starts with `lookup(`, its text is `lookup(q: "1") → {answer: 2}`, and `queryByText("Tool")` is null (the folded tool message is not rendered).
  - a response whose call is not in the messages → one button with text `response → {answer: 2}`, and the `Tool` message header is shown.
  - with `toolSpans` = a `Map` from the call part object (the same object passed in `input`) to `{ spanId: "b1", durationNs: 350_000_000 }` → the line ends with `· 350ms`.
  - with a match `{ spanId: "b1", durationNs: 1_000_000, error: "timeout" }` → the line contains `timeout`.
  - the existing "highlights tools that this span called" still passes.
- `span-detail.test.tsx`: replace "links a tool call to its response" by "folds a tool response into its call's line": same data; the Span region has one button whose text starts with `lookup(` and contains `→ {status: "shipped"}` (read the test's argument object and write the exact expected string); clicking it shows the `Arguments` and `Result` regions. Add "shows the matched tool span's duration in Chat": `renderApp("/acme/support-bot/traces?trace=a1000000000000000000000000000001&span=b100000000000014")`, open the Chat tab if it is not the default (see how the file's other tests do it), and expect in the Span region a button whose text starts with `get_order_status(order_id: "1234") → {status: "shipped", eta: "2026-01-02"}` and ends with `· 350ms`.
- `transcript-view.test.tsx`, new cases on the support-bot trace (every tool-line assertion awaits, with `findBy…` or `waitFor`, because the line fills in when the detail loads):
  - "shows a tool turn as one line": the `[data-turn="b100000000000013"]` row has a line whose text is exactly `get_order_status(order_id: "1234") → {status: "shipped", eta: "2026-01-02"} · 350ms`; no `Arguments` region in the Transcript before a click.
  - "a click on the tool line selects the span and opens it": the URL gets `span: "b100000000000013"` and the `Arguments` and `Result` regions are inside that row.
  - "an LLM turn's tool call shows the tool span's duration": the `[data-turn="b100000000000012"]` row has a line whose text is exactly `get_order_status(order_id: "1234") · 350ms`.
  - "the next LLM turn shows the response as its own line": the `[data-turn="b100000000000014"]` row has a line starting with `get_order_status → {status: "shipped"`.
  - "a failed tool turn shows its error on the line": `import traceFixture from "../../../../fixtures/api/responses/traces/a1000000000000000000000000000001.json"` (check the relative path the way `span-detail.test.tsx` imports its fixtures); `server.use(http.get("/api/v1/projects/:project/traces/:traceId", () => HttpResponse.json({ ...traceFixture, spans: traceFixture.spans.map((row) => row.span.span_id === "b100000000000013" ? { ...row, span: { ...row.span, status: "error", status_message: "1 failed, 47 passed" } } : row) })))` (check the exact route pattern and response shape in `fixtures/api` / the MSW handlers and adapt; leave the span-detail endpoints untouched); the tool row's line contains `1 failed, 47 passed` and has an image named `Failed`.
  - "Full width hides the side pane and is remembered": start on `/acme/support-bot/traces?trace=a1000000000000000000000000000001&mode=transcript` (no `span`, so the summary shows); `findByRole("region", { name: "Trace summary" })` exists; click the `Full width` button: its `aria-pressed` is `"true"`, `queryByRole("region", { name: "Trace summary" })` is null, the `Transcript` region is still there, and ``localStorage.getItem(`${STORAGE_PREFIX}transcript-wide`)`` is `"1"`; unmount and render the same URL again: the summary is still absent and the button pressed; click it again: the summary is back and the key is `null`. Add ``afterEach(() => localStorage.removeItem(`${STORAGE_PREFIX}transcript-wide`))`` with `STORAGE_PREFIX` imported from `@/product`.

**Done when:** `pnpm --dir web exec vitest run src/lib/tool-calls.test.ts src/components/payload src/components/trace 'src/routes/$org/$project/sessions' 'src/routes/$org/$project/traces' src/test --maxWorkers=8` passes; `pnpm --dir web exec tsc --noEmit -p .` passes; Biome is clean on the touched files; `rg -n "ToolCallCard|tool-call-card|NO_LINKS|NO_TOOL_LINKS" web/src` finds nothing.

**Commit:** `Show tool calls as one line with the response folded in`

---

### P3: Spec, decision, changelog, docs (main session)

Already drafted in the worktree: `docs/spec/11-ui.md` (§ Trace view: the Transcript sentence and the *Chat* bullet), `docs/mvp-decisions.md` (D-C-32 after D-C-31), `CHANGELOG.md` (`### Changed`), `website/src/content/docs/docs/tracing/traces.mdx`. The main session checks them against the code after P2, then runs `just check-web` and `just check-docs` once.

**Commit:** `Document compact tool calls`
