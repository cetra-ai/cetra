# Trace page: readable detail, rendered content, Tree/Timeline switch

Plan for Haiku implementers, one phase at a time. Read the whole of your phase before editing.

## Conventions (every phase)

- **Never guess.** If something in your phase is unclear, contradicts the code, or a step fails in a way the phase does not explain, STOP. Make no further edits. Your final message must start with `QUESTION:` followed by the question and what you found. You will be resumed with an answer.
- Work directly in `/home/sebastien/travaux/llm-observer` on `main`. Do not create branches or worktrees. Do not commit, stash, reset or switch branches.
- Edit only the files your phase lists. Other phases run at the same time on other files.
- Before any node command: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
- Web checks, from `web/`: `./node_modules/.bin/vitest run <paths>`, `npx tsc -b` (must print no errors). Biome from the repo root: `node_modules/.bin/biome check --write <paths>` (a "configuration schema version does not match" line is informational only). Delete `web/.vitest/` if a run creates it.
- AGENTS.md rules apply: no placeholders, no compatibility shims, tests for every behaviour change.
- The root font size is **13px**, so Tailwind's rem sizes are small (`text-sm` ≈ 11.4px, `text-xs` ≈ 9.75px). Use the explicit pixel sizes this plan gives (`text-[13px]` etc.).
- Final report: files changed, tests added, check results, anything not done.

## Decisions (already made, do not revisit)

| # | Decision |
|---|---|
| TP-1 | The trace's left pane has three modes: **Tree** (names with duration, tokens, cost; no time axis), **Timeline** (today's combined tree + time-axis grid) and **Transcript** (unchanged). Tree is the default. The URL `mode` param accepts `tree`, `timeline`, `transcript`. |
| TP-2 | Split defaults: Tree and Transcript use the `trace` split with the tree pane at **40%**; Timeline uses a separate `trace-timeline` split at **60%**, so each mode remembers its own ratio. |
| TP-3 | Detail text is readable: body 13px, labels 12px, titles 15–16px. |
| TP-4 | Text in "Pretty" mode is rendered as Markdown (GitHub flavour) with highlighted code blocks; "Raw" shows the plain text. Raw HTML in content is never rendered (no `rehype-raw`). Links open in a new tab with `rel="noopener noreferrer"`. |
| TP-5 | New web dependencies, exact versions: `react-markdown` **10.1.0**, `remark-gfm` **4.0.1**, `rehype-highlight` **7.0.2** (it brings `lowlight` and `highlight.js`). Code colours come from the palette tokens, so every palette and theme styles code. |
| TP-6 | Metric chips (an icon, a value, optional tooltip) are one shared component used by the span header and the trace header. |
| TP-7 | The trace header gets previous/next trace buttons when the page that opened the trace provides them (the Traces list). |
| TP-8 | Spec 11 is updated for TP-1/TP-2 (it currently says one combined grid, D126, and a 55% tree pane, D125); a decision D-C-10 is added to `docs/mvp-decisions.md` track C. The coordinator does this, not the phases. |

## Waves

- **Wave A (parallel):** TP1 (demo data) ∥ TP2 (Tree/Timeline) ∥ TP3 (Markdown and messages) ∥ TP4 (span header and detail).
- **Wave B:** TP5 (trace header, previous/next).
- **Wave C:** coordinator: docs (TP-8), `just check-web`, browser check.

---

## TP1 — Realistic demo payloads

**Edit only:** `web/src/mocks/generated/dataset.ts` and its tests (`dataset*.test.ts`, `handlers.test.ts` where they assert payload text).

The large mock (`?mock=large`) gives spans inputs like `evaluator 8` and outputs like `ok` (see the span payload code near the `textPayload`/`{ format: "json" … }` lines, around line 460). Replace them with realistic, deterministic content chosen by span kind and index (no randomness beyond the dataset's existing seeded generator):

- **llm:** input `format: "messages"`: a system message (one short instruction), a user message (a question); output `format: "messages"`: one assistant message whose text is Markdown with a `##` heading, a short paragraph, a bullet list and one fenced code block (` ```python ` or ` ```ts `, alternating). Use at least 6 different question/answer pairs about common support-bot topics (refunds, password reset, API rate limits, exporting data, webhook setup, billing), cycling by index.
- **tool:** input JSON `{ "name": "<tool name>", "arguments": { … } }`, output JSON with a plausible result object.
- **retriever:** input text (the query); output JSON `{ "documents": [ { "id", "score", "text" } × 3 ] }`.
- **embedding:** input text; output JSON `{ "dimensions": 1536, "model": "text-embedding-3-small" }`.
- **guardrail:** output JSON `{ "passed": true|false, "reasons": [ … ] }`.
- **evaluator:** output JSON `{ "score": <0..1>, "reasoning": "<one sentence>" }`.
- **agent / chain / root spans:** input text (the user's question), output Markdown (a short answer with a list).
- Keep the `Message` / `Payload` shapes exactly as `web/src/api/schema.gen.ts` defines them; read them first. If a kind above does not exist in the generator, skip it and say so.
- Keep list previews (`input_preview` / `output_preview`) consistent: the first line of the text, truncated as today.

**Tests:** update assertions that expected the old strings; add one test that an llm span's detail has `messages` input and a Markdown output containing a fenced code block.

---

## TP2 — Tree / Timeline switch

**Edit only:** `web/src/lib/search/traces.ts` (+ test), `web/src/components/trace/tree-toolbar.tsx`, `web/src/components/trace/tree-grid.tsx` (+ test), `web/src/components/trace/trace-view.tsx`, `web/src/components/trace/split-pane.tsx` (+ test), and route tests that assert the mode toggle.

1. `TRACE_MODES = ["tree", "timeline", "transcript"]` (order matters for the toggle). Parsing keeps rejecting unknown values.
2. `TreeToolbar` `ModeToggle`: three buttons **Tree**, **Timeline**, **Transcript**, same style and `aria-checked` pattern as today.
3. `TreeGrid` gets `timeline: boolean` (from `mode === "timeline"`):
   - `true`: exactly today's grid.
   - `false`: no time-axis column and no time-axis header/ticks/zoom/drag-zoom UI. Read the file and gate every timeline-only element (the `timelineRef` column, ticks/`marks`, `SpanBar`, zoom state and handlers, the zoom controls passed to the toolbar). The grid template becomes `minmax(160px, 1fr) 72px 64px 72px` (name, duration, tokens, cost), and the name-column resize handle is hidden (the name column fills the space). Keyboard navigation, selection, expand/collapse and live highlights behave the same in both.
   - If a timeline-only behaviour cannot be cleanly gated (e.g. state that other features read), STOP and ask.
4. `trace-view.tsx`: pass `timeline={active === "timeline"}`; the `toggle-mode` shortcut cycles tree → timeline → transcript → tree; the `SplitPane` `name` is `active === "timeline" ? "trace-timeline" : "trace"`.
5. `split-pane.tsx` `PANES`: `trace.startSize` 55 → **40**; add `"trace-timeline": { start: "trace-timeline-tree", end: "trace-timeline-detail", startSize: 60 }` and add it to the `SplitName` type wherever the union is defined.
6. **Tests:** mode parsing accepts `timeline`; Tree mode renders no time-axis column (no `SpanBar`) and Timeline does; the toggle shows three options and switches; the shortcut cycles three modes; split defaults 40 and 60.

---

## TP3 — Markdown, code highlighting, message cards

**Edit only:** `web/package.json` (and the lockfile via `pnpm install`), new `web/src/components/payload/markdown.tsx` (+ test), new `web/src/components/payload/show-more.tsx` (+ test), `web/src/components/payload/payload-view.tsx`, `message-view.tsx`, `part-view.tsx`, `chat-view.tsx`, `json-view.tsx`, `format-switch.tsx`, their tests, and `web/src/styles.css` (only the code-colour block below).

1. Dependencies: add `react-markdown` `10.1.0`, `remark-gfm` `4.0.1`, `rehype-highlight` `7.0.2` to `web/package.json` dependencies with exact versions (no `^`), then `pnpm install` from the repo root. If `pnpm` refuses (e.g. release-age), STOP and ask.
2. `Markdown({ text })` in `markdown.tsx`: `react-markdown` with `remarkPlugins={[remarkGfm]}` and `rehypePlugins={[rehypeHighlight]}`. Read `node_modules/rehype-highlight/readme.md` for how unknown languages and auto-detection behave; code blocks without a language must not be auto-detected (plain). If the readme leaves that unclear, STOP and ask. Never add `rehype-raw`. Element styles through the `components` prop:
   - `h1` `mt-3 mb-1.5 text-[17px] font-semibold`; `h2` `mt-3 mb-1.5 text-[15px] font-semibold`; `h3`–`h6` `mt-2 mb-1 text-[13px] font-semibold`.
   - `p` `my-1.5 text-[13px] leading-relaxed`; `ul` `my-1.5 list-disc pl-5`; `ol` `my-1.5 list-decimal pl-5`; `li` `my-0.5 text-[13px] leading-relaxed`.
   - `a` `text-primary underline underline-offset-2`, `target="_blank" rel="noopener noreferrer"`.
   - inline `code` `rounded bg-muted px-1 py-0.5 font-mono text-[12px]`.
   - fenced code (`pre`): a `CodeBlock` with a header bar (`flex items-center justify-between border-b px-3 py-1 text-[11px] text-muted-foreground`) showing the language (from the `language-…` class; "text" if none) and a `CopyButton` (`@/components/copy-button`) with the code text, then `pre` `overflow-auto p-3 font-mono text-[12px] leading-relaxed`; the whole block `my-2 rounded-md border bg-surface`.
   - `table` `my-2 w-full border-collapse text-[12px]`, `th`/`td` `border px-2 py-1 text-left`; `blockquote` `my-2 border-l-2 pl-3 text-muted-foreground`; `hr` `my-3 border-border`.
3. `styles.css`: one block of `.hljs-…` rules using palette tokens, so code follows every palette: `keyword, selector-tag, built_in-ish keywords` → `var(--cat-4)`; `string, attr-value, regexp` → `var(--cat-2)`; `number, literal` → `var(--cat-3)`; `title, title.function_, section` → `var(--cat-1)`; `built_in, type, class` → `var(--cat-5)`; `attr, property, variable, params` → `var(--cat-6)`; `comment, quote, meta` → `var(--muted-foreground)` (comments italic). Use the class names highlight.js actually emits (check its CSS themes in `node_modules/highlight.js/styles/` for the list).
4. `ShowMore({ children, maxHeight = 480 })` in `show-more.tsx`: measures its content with `ResizeObserver`; when taller than `maxHeight`, clips it with a fade at the bottom and a "Show more" button (`text-[12px] text-primary`) that expands it ("Show less" collapses). Under the test setup's inert `ResizeObserver` stub nothing is clipped; test the clipping by stubbing a height.
5. `payload-view.tsx`:
   - text payloads: "pretty" → `<ShowMore><Markdown text={…} /></ShowMore>`; "raw" → today's `pre` (now `text-[12px]`). The format switch must offer pretty/raw for text (read `format-switch.tsx`; if it hides itself for text today, show it).
   - JSON: unchanged behaviour, wrapped in `ShowMore`; text sizes `text-[12px]`.
   - the label row: label `text-[12px] font-medium`, badges and counts `text-[11px]`.
   - `MessageList`: each message renders through `MessageView` (the same card as the Chat tab) instead of its own role + plain text, so Input/Output and Chat look the same. If `MessageView` needs props this file doesn't have (e.g. `links`), STOP and ask.
6. `message-view.tsx`: a card: `rounded-md border bg-card` with a header row `flex items-center gap-2 border-b px-3 py-1.5 text-[12px]` holding a role icon (lucide `User` for user, `Bot` for assistant, `Settings2` for system, `Wrench` for tool, `MessageSquare` otherwise; `size-3.5 text-muted-foreground`), the capitalised role (`font-medium`), the name, and the finish-reason badge; body `px-3 py-2`. Keep the existing line-limit expand logic or replace it with `ShowMore` (one or the other, not both; prefer `ShowMore`, and delete the line counting if you do). Keep the role border colour as a 2px left border on the card.
7. `part-view.tsx`: text parts render `<Markdown text={…} />`; other part types keep their behaviour with `text-[12px]`. `chat-view.tsx`: the "Output" divider and section labels `text-[12px]`.
8. **Tests:** Markdown renders a heading, a list, a link with `target="_blank"` and `rel`, an inline code, and a fenced block with its language label and copy button; raw HTML in the text is shown as text, not parsed into elements; ShowMore clips and expands; payload text renders Markdown in pretty and plain text in raw; a message card shows the role label. Update existing payload/chat tests for the new markup without weakening what they check.

---

## TP4 — Span header and detail typography

**Edit only:** `web/src/components/trace/span-header.tsx`, `web/src/components/trace/span-detail.tsx`, `web/src/components/trace/io-tab.tsx`, new `web/src/components/trace/metric-chip.tsx` (+ test), and their tests.

1. `MetricChip({ icon, children, label })` in `metric-chip.tsx`: `inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-[12px]`, the lucide `icon` at `size-3.5 text-muted-foreground` (aria-hidden), the value in `num text-foreground`, and `aria-label={label}` on the chip when given. It forwards a `ref`/props so it can be a `TooltipTrigger asChild` child.
2. `SpanHeader` layout (keep every piece of information it shows today):
   - Row 1 (`flex items-center gap-2`): `KindIcon` (make it `size-4` here via a wrapper or prop if `KindIcon` allows; if not, leave its size), the span name `text-[15px] font-semibold`, a status pill (`ok` → "OK" `rounded px-1.5 text-[11px] font-medium text-status-ok bg-status-ok/12`; `error` → "Error" with `status-error`; `unset` → no pill), the status message, and `IdText` pushed right (`ml-auto`).
   - Row 2: chips: start time (`TimeText`, icon `Clock`), duration (`Timer`), TTFT when present (`Zap`), tokens summary (`Coins`), cost (`DollarSign`, keep its tooltip), model (`Sparkles`, keep the request/response tooltip).
   - Row 3 (`text-[12px] text-muted-foreground`, wrap): provider, operation, service, environment (as the env pill from `environmentClasses` in `@/lib/colors`, same classes as the trace list's env cell), release, session and user links, the badges (truncated, detached, conventions) at `text-[11px]`.
   - Header padding `px-4 py-3`, gap between rows `gap-2`.
   - Check `SpanDetail` has `start_time`; if it does not, drop that chip and say so.
3. `span-detail.tsx`: tab triggers `text-[12px]`, list height `h-9`, content padding `px-4 pb-4 pt-3`.
4. `io-tab.tsx`: gap between sections `gap-4`; the collapsible "System instructions" / "Tool definitions" buttons `text-[12px] font-medium` with a chevron (`ChevronRight` rotating when open).
5. **Tests:** header shows name, OK/Error pill, the chips with their values (duration, tokens, cost), env pill, session/user links; existing span-detail tests keep passing (update selectors only for changed markup).

---

## TP5 — Trace header, previous/next

Runs after Wave A. **Edit only:** `web/src/components/trace/trace-header.tsx` (+ test), `web/src/components/trace/trace-view.tsx` (props only), `web/src/routes/$org/$project/traces/index.tsx` (+ test).

1. `TraceHeader` layout (keep all its information and actions):
   - Row 1: trace name `text-[15px] font-semibold`, status pill (same as TP4), `IdText`, live indicator; actions on the right as today.
   - Row 2: `MetricChip`s (from TP4's `metric-chip.tsx`): start time, duration, tokens (keep its breakdown tooltip), cost, span count (`ListTree`); then session/user links, environment pill, tags as pills (`categoryClasses(tag).pill`), score badges as today.
   - Padding `px-4 py-3`.
2. Previous/next: `TraceView` and `TraceHeader` get optional `onPrevious?: () => void` and `onNext?: () => void`. When defined, the header shows two icon buttons (lucide `ChevronUp` "Previous trace", `ChevronDown` "Next trace", `variant="ghost" size="icon-xs"`) before the close button; a missing handler hides that button. Keyboard: `k` / `j` while the trace has focus — check `useShortcut` bindings in `@/components/command/` for existing ids for previous/next (e.g. list navigation); reuse them if they exist, otherwise STOP and ask before adding shortcut ids.
3. Traces route: pass `onPrevious` / `onNext` that open the previous / next row of the loaded list (the same rows the table shows, in display order), keeping the current search params, exactly as clicking that row would. At the first/last loaded row, pass `undefined` for that side.
4. **Tests:** buttons appear only with handlers; clicking Next on the Traces page opens the next row's trace; at the last row Next is hidden.
