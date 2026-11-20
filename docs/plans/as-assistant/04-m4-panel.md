# AS M4 · Panel and closeout — plan

Track AS milestone M4 (design: [`00-design.md`](00-design.md), D-AS-3, D-AS-6, D-AS-7, D-AS-9, D-AS-14, D-AS-18). M4 puts the assistant in the web UI and closes the track:
- the assistant panel in the project shell: history, transcript, streamed answers, tool steps, draft cards, Stop, thumbs, context chips;
- Apply on drafts: the list pages, the SQL console, the dashboard editor, the evaluator and online-rule forms open prefilled; nothing is saved until the user saves;
- the org setting page with the tool-calling test and the provider notice;
- a Rust end-to-end test, the docs page and its screenshots, the CHANGELOG lines, spec 11, the roadmap row.

**Implementer:** Haiku, one phase per agent, in the worktree `.claude/worktrees/as-assistant-6899aa` on branch `claude/as-assistant-6899aa`. If this plan and the code disagree, **STOP and report**; do not guess.

**Checks (the user's rule, 2026-11-20):** no phase compiles, tests, type-checks or builds anything. Allowed per phase: `cargo fmt --all`, `pnpm --dir web exec biome format --write <files>`, `pnpm --dir web gen:routes` (route tree only, AS4.2). When AS4.8 is committed, the milestone runs `just check` once and fixes what fails.

**Facts:** the web code facts this plan relies on, with file and line references, are in `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/as-m4-web-facts.md` (§ 0 to § 12). Every phase reads the sections it names.

## Prerequisites

Each command must print at least one line; otherwise STOP and report.

```bash
grep -n "post_assistant_turn" crates/server/openapi.json
grep -n "AssistantStatus" web/src/api/schema.gen.ts
grep -n "LATER_TRACKS" web/src/mocks/writes.test.ts
grep -n "export function useSqlTabs" web/src/lib/sql/tabs.ts
grep -n "export function EvaluatorForm" web/src/components/evaluators/evaluator-form.tsx
grep -n "export function RuleForm" web/src/components/online-rules/rule-form.tsx
grep -n "export function NewDashboardDialog" web/src/components/dashboard/dashboard-dialogs.tsx
grep -n "export function reduceShortcut" web/src/lib/shortcuts.ts
grep -n "playground_run_end_to_end" crates/cli/tests/e2e.rs
```

**Read first** (every phase): `AGENTS.md`; `docs/plans/as-assistant/00-design.md` (D-AS-3, D-AS-6, D-AS-7, D-AS-9, D-AS-14, D-AS-18); this file up to § AS4.1, then your phase; `docs/spec/09-api.md` § Assistant (routes, SSE events, statuses); `docs/spec/11-ui.md` (conventions).

## What it is, and what it is not

When M4 is done:
- A member opens the panel from the top bar or with `Ctrl/Cmd+I` on any project page. It sits on the right, resizable, and stays open while they navigate, project settings included.
- The panel lists their conversations, starts a new one, renames and deletes one, and reopens the last one of the project.
- They type a question. The page's context shows as chips they can remove. The answer streams as Markdown, with tool steps collapsed, draft cards, Stop, and thumbs.
- Apply on a draft opens its target prefilled:
  - a filter: the list page with the filter in its URL;
  - SQL: a new console tab;
  - a dashboard: the new-dashboard dialog, or the dashboard's editor with the draft definition;
  - an evaluator or a rule: its new form.
- An org admin turns the assistant on in Organization settings → Assistant. They pick a connection and a model, and saving runs the tool-calling test.
- An off assistant says why: off in the org, with a link for admins; no connection; restore mode.
- `just e2e` runs a turn against the mock provider over ingested data and gets an answer and a filter draft; a vitest test applies a filter draft from the panel.
- The docs have an Assistant page with two screenshots. CHANGELOG, spec 11 and the roadmap row are updated.

Out of scope:
- a browser end-to-end test (the user chose Rust e2e plus vitest, 2026-11-20);
- the design's open questions Q1–Q3;
- V's M7 playground page (it may reuse `lib/event-stream.ts`, below);
- any server change except the Rust e2e test.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AS-6 | Drafts, never writes; Apply opens the target prefilled; evaluator and rule cards show the full template, mapping and filter |
| D-AS-7 | Markdown without HTML or images; app links in-app; other links show their URL and open on click in a new tab with `noopener noreferrer` |
| D-AS-9 | The turn route and its SSE events |
| D-AS-14 | Page context: at most 8 KiB, shown as removable chips |
| D-AS-18 | The panel |
| D-AS-26 (new) | **Panel layout (user, 2026-11-20).** A right-hand resizable column that shrinks the page, mounted above the project/settings sidebar switch so it stays open across navigation. Default 30 % of the width, between 20 % and 50 %. The width and the open state persist in `localStorage`. Below 1024 px it is a full-width sheet over the page. |
| D-AS-27 (new) | **Shortcut (user, 2026-11-20).** `Ctrl/Cmd+I` toggles the panel, including while typing; it calls `preventDefault`. `Esc` in the panel moves focus back to the page and does not close it. Listed in the shortcuts help. |
| D-AS-28 (new) | **Apply never overwrites (user, 2026-11-20).** Filters go in the URL. The other drafts are handed over in memory (`lib/assistant/handoff.ts`), and the target takes them once. SQL opens a new console tab, even when the console is open. A new-dashboard draft opens the new-dashboard dialog prefilled. An edit draft opens that dashboard's editor with the draft definition at the draft's `version`, so a changed dashboard gets the existing 409 "changed" message. Evaluators and rules open their new form prefilled. A reload drops a handed-over draft. Nothing is saved until the user saves. |
| D-AS-29 (new) | **Page context vocabulary.** `page` is one of `trace`, `session`, `user`, `traces`, `spans`, `sessions`, `users`, `sql`, `dashboard`, `evaluator`, `online_rule`, `dataset`, `experiment`, `settings`, `other`, with the fields of § Page context. Each field except `page` is a chip. The SQL text and filter text are included. Over 8 KiB, the longest string fields are cut to fit, and `truncated: true` is added. |
| D-AS-30 (new) | **Shared POST event-stream client.** `web/src/lib/event-stream.ts` `postEventStream` reads a POST answered with `text/event-stream` through `fetch` and a `ReadableStream`. It is generic, so V's M7 playground reuses it. |
| D-AS-31 (new) | **E2E (user, 2026-11-20).** A Rust e2e runs a turn over ingested data with the mock provider and checks the stream, the stored draft and the trace; the panel's Apply is covered by vitest. No browser test. |

The user's answers (2026-11-20): resizable column (D-AS-26), `Ctrl/Cmd+I` (D-AS-27), prefill and never overwrite (D-AS-28), Rust e2e plus vitest (D-AS-31). D-AS-29 and D-AS-30 are this plan's proposals, recorded in AS4.8.

## Versions

No new dependency. The panel uses `react-markdown` 10.1.0, `remark-gfm` 4.0.1, `rehype-highlight` 7.0.2 and `react-resizable-panels` 4.21.0, already in `web/package.json` (re-check and report the versions found).

## Contracts

### Event types (`web/src/lib/assistant/types.ts`, AS4.1)

```ts
import type { Schema } from "@/api/schema";

export type AssistantMessage = Schema<"AssistantMessage">;
export type AssistantPart = Schema<"AssistantPart">;
export type AssistantStatus = Schema<"AssistantStatus">;
export type DraftKind = Schema<"AssistantDraftKind">;

/** The turn route's SSE events (spec 09 § Assistant). The OpenAPI does not describe them. */
export type TurnEvent =
  | { event: "start"; data: { turn_id: number; trace_id: string } }
  | { event: "text"; data: { delta: string } }
  | { event: "tool_call"; data: { id: string; name: string; arguments: unknown } }
  | {
      event: "tool_result";
      data: { id: string; ok: boolean; summary: { size_bytes: number; rows?: number; error_code?: string } };
    }
  | { event: "draft"; data: { id: string; kind: DraftKind; draft: unknown } }
  | {
      event: "done";
      data: {
        finish_reason: string;
        usage: { input: number; output: number };
        cost_usd: number | null;
        steps: number;
      };
    }
  | { event: "error"; data: { class: "retryable" | "pause" | "permanent"; message: string } };

/** Parses one SSE block's name and JSON data into a TurnEvent, or `null` (unknown name, bad JSON, wrong shape, `ping`). */
export function parseTurnEvent(name: string, data: string): TurnEvent | null;

export type FilterDraft = {
  view: "traces" | "spans" | "sessions" | "users";
  filter?: string;
  search?: string;
  range?: "15m" | "1h" | "6h" | "24h" | "7d" | "30d";
  from?: string;
  to?: string;
  include_eval: boolean;
  include_experiments: boolean;
};
export type SqlDraft = {
  query: string;
  params: Record<string, unknown>;
  range?: string;
  from?: string;
  to?: string;
  columns: { name: string; type: string }[];
};
export type DashboardDraft = {
  dashboard_id?: string;
  version?: number;
  name: string;
  description?: string;
  definition: Schema<"Definition">;
};
export type EvaluatorDraft = {
  name: string;
  kind: "llm_judge" | "check";
  config: Record<string, unknown>;
  output_score: Schema<"OutputScoreBody">;
};
export type RuleDraft = {
  name: string;
  evaluator_id: string;
  target: "trace" | "span";
  filter?: string;
  variable_mapping: Record<string, string>;
  sample_rate: number;
  budget_usd?: number;
  budget_period?: "day" | "month";
  enabled: boolean;
};
export type Draft =
  | { kind: "filter"; draft: FilterDraft }
  | { kind: "sql"; draft: SqlDraft }
  | { kind: "dashboard"; draft: DashboardDraft }
  | { kind: "evaluator"; draft: EvaluatorDraft }
  | { kind: "online_rule"; draft: RuleDraft };

/** Runtime check of a draft value (the generated type is `Record<string, never>`). `null` when the shape is wrong. */
export function parseDraft(kind: DraftKind, value: unknown): Draft | null;
```

`parseDraft` checks every required field's type and every enum value listed above, and drops unknown keys. `output_score` is checked against the required fields of the generated `OutputScoreBody` type (read it); `definition` is checked only to be an object with a `widgets` array; the target page's own validation does the rest.

### Event-stream client (`web/src/lib/event-stream.ts`, AS4.1)

```ts
export type StreamBlock = { event: string; data: string };

/** Problem body or network failure before the stream starts. */
export class StreamRequestError extends Error {
  readonly status: number; // 0 for a network failure
  readonly problem: unknown; // the parsed Problem body, or undefined
  readonly retryAfter?: number; // seconds, from a `Retry-After` header holding an integer
}

/**
 * POSTs `body` as JSON to `url` (same origin, `credentials: "same-origin"`, `Accept: text/event-stream`).
 * A non-2xx answer: calls `reportUnauthorized(pathname)` on 401, then throws `StreamRequestError`.
 * A 2xx answer: reads the body, splits blocks on a blank line, joins `data:` lines with "\n",
 * takes the last `event:` line (default "message"), ignores comment lines (`:`) and `id:`/`retry:`,
 * and calls `onBlock` for each block in order. Resolves when the body ends.
 * Aborting `signal` rejects with the abort's `DOMException` (`name === "AbortError"`).
 */
export async function postEventStream(
  url: string,
  body: unknown,
  options: { signal: AbortSignal; onBlock: (block: StreamBlock) => void },
): Promise<void>;
```

### Hand-off store (`web/src/lib/assistant/handoff.ts`, AS4.5)

```ts
type Handed =
  | { kind: "sql"; draft: SqlDraft }
  | { kind: "dashboard"; draft: DashboardDraft }
  | { kind: "evaluator"; draft: EvaluatorDraft }
  | { kind: "online_rule"; draft: RuleDraft };

/** Hands a draft to the next target of its kind in project `ref`; replaces an untaken one of the same kind. */
export function handDraft(ref: string, handed: Handed): void;
/** The draft of `kind` handed in `ref`, without taking it, or `undefined`. */
export function peekDraft<K extends Handed["kind"]>(ref: string, kind: K): Extract<Handed, { kind: K }>["draft"] | undefined;
/** Takes (and removes) the draft of `kind` handed in `ref`, or `undefined`. */
export function takeDraft<K extends Handed["kind"]>(ref: string, kind: K): Extract<Handed, { kind: K }>["draft"] | undefined;
/** Calls `listener` after each `handDraft` in `ref`; returns the unsubscribe function. */
export function subscribeDrafts(ref: string, listener: () => void): () => void;
```

It is in-memory only (a module `Map`), so a reload drops it (D-AS-28).

### Panel store (`web/src/lib/assistant/panel.ts`, AS4.3)

Module store with `useSyncExternalStore`, in the style of `web/src/lib/list-layout.ts` (every `localStorage` access in try/catch, in-memory fallback):

```ts
export function usePanelOpen(): boolean;
export function setPanelOpen(open: boolean): void;
export function togglePanel(): void;
/** The conversation shown for project `ref`, persisted per project. */
export function useCurrentConversation(ref: string): string | undefined;
export function setCurrentConversation(ref: string, id: string | undefined): void;
export function readPanelSize(): number; // percent, default 30, clamped to [20, 50]
export function writePanelSize(percent: number): void;
```

Storage keys (`STORAGE_PREFIX` from `src/product.ts` is `cetra.`): `cetra.assistant.open` (`"1"`/`"0"`), `cetra.assistant.size` (number), `cetra.assistant.conversation.<org>/<project>` (UUID).

### Page context (`web/src/lib/assistant/context.ts`, AS4.4)

```ts
export type PageContext = { page: PageKind } & Record<string, string | number | boolean>;
export type PageKind =
  | "trace" | "session" | "user" | "traces" | "spans" | "sessions" | "users" | "sql"
  | "dashboard" | "evaluator" | "online_rule" | "dataset" | "experiment" | "settings" | "other";

/** The context of the page at `pathname` + `search` in project `org`/`project`. Pure: no storage reads except `sqlQuery` passed in. */
export function pageContext(input: {
  org: string;
  project: string;
  pathname: string;
  search: Record<string, unknown>;
  sql?: { query: string; params: Record<string, string> };
}): PageContext;

/** Removes the chip `key`, never `page`. */
export function withoutField(context: PageContext, key: string): PageContext;

/** Fits the context into 8,192 bytes of compact JSON (UTF-8): cuts the longest string field by half until it fits, adds `truncated: true`; drops every non-`page` field as a last resort. */
export function fitContext(context: PageContext): PageContext;

export const CONTEXT_MAX_BYTES = 8192;
```

Fields per kind:

| page | when | fields |
|---|---|---|
| `trace` | `/traces/<32 hex>` path, or `?trace=` on `/traces` or `/spans` | `trace_id`, `span_id` (from `?span=`) |
| `session` | `/sessions/<id>` | `session_id` |
| `user` | `/users/<id>`, or `?user=` on `/users` | `user_id` |
| `traces`, `spans`, `sessions`, `users` | the list index with no open item | `filter`, `search`, `range` or `from`+`to`, `include_eval` (from `eval`), `include_experiments` (from `exp`), `view` |
| `sql` | `/sql` | `query`, `params` (JSON text of the params), `range` or `from`+`to` |
| `dashboard` | `/dashboard` or `/dashboard/<id>` | `dashboard_id` (absent for the overview), `filter`, `range` or `from`+`to` |
| `evaluator` | `/evaluators/<uuid>` | `evaluator_id` |
| `online_rule` | `/online-rules/<uuid>/edit`, or `?rule=` on `/online-rules` | `rule_id` |
| `dataset` | `/datasets/<uuid>` | `dataset_id` |
| `experiment` | `?experiment=` on `/experiments` | `experiment_id` |
| `settings` | project settings paths (`isProjectSettingsPath`) | none |
| `other` | anything else | `path` (the pathname without `/<org>/<project>`) |

Fields whose value is empty or absent are left out. The list `range` is the search's `range`, else `from`/`to`, else `"24h"` (the list default).

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| AS4.1 Foundations | `web/src/lib/event-stream.ts` (new) + `.test.ts`, `web/src/lib/assistant/types.ts` (new) + `.test.ts`, `web/src/api/queries-assistant.ts` (new), `web/src/lib/roles.ts`, `web/src/lib/roles.test.ts`, `web/src/test/turn-stream.ts` (new), `web/src/mocks/assistant.ts` (new) | — |
| AS4.2 Org setting page | `web/src/routes/$org/settings/assistant.tsx` (new) + `.test.tsx`, `web/src/components/settings/settings-nav.ts`, `web/src/components/settings/settings-home.tsx` (+ its test if it lists cards), `web/src/components/settings/connection-test.tsx` (export the result display), `web/src/routes/$org/settings/llm-connections.tsx` (delete copy, invalidation) and its `.test.tsx`, `web/src/mocks/writes.ts`, `web/src/mocks/writes.test.ts`, `fixtures/api/static/` (only if a static answer file is needed), `web/src/routeTree.gen.ts` | AS4.1 |
| AS4.3 Panel shell | `web/src/hooks/use-narrow-viewport.ts` (new), `web/src/components/shell/project-sidebar.tsx`, `web/src/lib/assistant/panel.ts` (new) + `.test.ts`, `web/src/components/assistant/assistant-panel.tsx` (new), `web/src/components/assistant/conversation-menu.tsx` (new), `web/src/components/assistant/unavailable.tsx` (new), `web/src/components/assistant/assistant-button.tsx` (new), `web/src/components/assistant/assistant-panel.test.tsx` (new), `web/src/components/shell/top-bar.tsx`, `web/src/routes/$org/$project/route.tsx`, `web/src/lib/shortcuts.ts`, `web/src/lib/shortcuts.test.ts`, `web/src/components/command/palette.tsx` | AS4.1 |
| AS4.4 Turns in the panel | `web/src/lib/assistant/context.ts` (new) + `.test.ts`, `web/src/lib/assistant/transcript.ts` (new) + `.test.ts`, `web/src/components/assistant/transcript.tsx` (new), `web/src/components/assistant/composer.tsx` (new), `web/src/components/assistant/tool-step.tsx` (new), `web/src/components/assistant/answer-markdown.tsx` (new), `web/src/components/payload/markdown.tsx` (export its `components` map only), `web/src/components/assistant/assistant-panel.tsx`, `web/src/components/assistant/turn.test.tsx` (new) | AS4.3 |
| AS4.5 Drafts and Apply | `web/src/lib/assistant/handoff.ts` (new) + `.test.ts`, `web/src/lib/assistant/apply.ts` (new) + `.test.ts`, `web/src/components/assistant/draft-card.tsx` (new), `web/src/components/assistant/transcript.tsx`, `web/src/lib/sql/tabs.ts`, `web/src/components/dashboard/dashboard-header.tsx`, `web/src/components/dashboard/dashboard-body.tsx`, `web/src/components/dashboard/dashboard-dialogs.tsx`, `web/src/components/assistant/tool-step.tsx`, `web/src/components/evaluators/evaluator-form.tsx`, `web/src/routes/$org/$project/evaluators/new.tsx`, `web/src/components/online-rules/rule-form.tsx`, `web/src/routes/$org/$project/online-rules/new.tsx`, `web/src/components/assistant/apply.test.tsx` (new) | AS4.4 |
| AS4.6 Rust e2e | `crates/cli/tests/e2e.rs` | — (server code is done) |
| AS4.7 Docs and screenshots | `website/src/content/docs/docs/platform/assistant.mdx` (new), `website/astro.config.mjs`, `website/src/content/docs/docs/evaluation/llm-connections.mdx`, `website/src/content/docs/docs/tracing/privacy-controls.mdx`, `website/src/content/docs/docs/platform/mcp.mdx`, `website/src/content/docs/docs/get-started/ui-tour.mdx`, `website/screenshots.json`, `web/src/mocks/browser.ts`, `web/src/mocks/assistant.ts` | AS4.5 |
| AS4.8 Closeout | `CHANGELOG.md`, `docs/spec/11-ui.md`, `docs/plans/as-assistant/00-design.md`, `docs/mvp-decisions.md` (D-AS-26 to D-AS-31 rows only), `docs/roadmap.md` (AS row only), this file (§ Deviations) | AS4.7 |

AS4.2 and AS4.3 may run in parallel after AS4.1 (their Edit lists do not overlap). AS4.6 may run at any time, in parallel with any web phase. The screenshots PNGs are written at the milestone check (`just docs-screenshots assistant-panel assistant-setting`), not by AS4.7.

**Shared rules for every phase.**
- Privacy (`AGENTS.md`): the web never logs message text, tool arguments or results, drafts or page context (`console.*` is not used for them).
- UI conventions: spec 11 and the components in `web/src/components/ui/`; text sizes `text-ui`/`text-body`/`text-title` as the neighbouring code uses; icons from `lucide-react`; every control has an accessible name; disabled controls use `DisabledReason` when a reason exists.
- Tests use the MSW mocks of `fixtures/api/` and override endpoints with `server.use` (`AGENTS.md`); never mock modules. `src/test/setup.ts` fails a test on any unhandled request.
- Stage your own files by path (never `git add -A`). Use the exact commit subject. No attribution lines.
- Format the files you changed (`pnpm --dir web exec biome format --write <files>`, `cargo fmt --all` for Rust). Run no build, type-check or test.

---

### AS4.1 Foundations

**Goal:** the shared client pieces: the POST event-stream reader, the assistant types and parsers, the API queries, `canUseAssistant`, and the turn-stream test helpers.

**Edit:** see the phase table.

**Read:** facts § 0, § 2, § 3, § 8; `web/src/lib/sse.ts`; `web/src/test/event-source.ts` (its block parser); `web/src/api/session.ts`; `web/src/api/queries-settings.ts` (query style); `web/src/lib/roles.ts` and its test.

**Steps:**
1. `web/src/lib/event-stream.ts`: implement § Contracts › Event-stream client.
   - `fetch(url, {method: "POST", headers: {"Content-Type": "application/json", Accept: "text/event-stream"}, credentials: "same-origin", body: JSON.stringify(body), signal})`.
   - On `!response.ok`: if `response.status === 401`, call `reportUnauthorized(new URL(url, window.location.origin).pathname)`; parse the body as JSON when its content type contains `json`, else `undefined`; read `Retry-After` (an integer number of seconds; anything else gives `undefined`); throw `new StreamRequestError(status, problem, retryAfter)`.
   - A network `TypeError` from `fetch` becomes `StreamRequestError(0, undefined)`; an `AbortError` is rethrown unchanged.
   - Read `response.body.getReader()`, decode with `TextDecoder` (`stream: true`), normalize `\r\n` to `\n`, split on `\n\n`, keep the incomplete tail. For each block: lines starting `:` are ignored; `event:` sets the name (one space after the colon trimmed); `data:` lines are joined with `\n`; a block with no `data:` line is skipped. Flush a final complete block at the end of the body.
2. `web/src/lib/assistant/types.ts`: implement § Contracts › Event types. `parseTurnEvent` returns `null` for `ping`, unknown names, invalid JSON, or a data object missing a required field of the listed type. `parseDraft` per § Contracts.
3. `web/src/api/queries-assistant.ts`, in the style of `queries-settings.ts`:
   - `assistantStatusQuery(org, project)` (`GET /api/v1/projects/{project}/assistant`), `assistantStatusKey`;
   - `conversationsQuery(org, project)` (`GET …/assistant/conversations`), `conversationsKey`;
   - `conversationQuery(org, project, id)` (`GET …/assistant/conversations/{id}`), `conversationKey`;
   - `orgAssistantQuery(org)` (`GET /api/v1/orgs/{org}/assistant`), `orgAssistantKey`;
   - `turnUrl(org, project, id): string` = `` `/api/v1/projects/${encodeURIComponent(projectRef(org, project))}/assistant/conversations/${id}/turns` `` (check how `lib/sse.ts` encodes the `{project}` path value and do the same).
4. `web/src/lib/roles.ts`: after `canRunPlayground`, add
   ```ts
   /** Mirrors `ProjectPerm::UseAssistant`: members and above, with a session (D-AS-4). */
   export function canUseAssistant(role: OrgRole | undefined): boolean {
     return atLeast(role, "member");
   }
   ```
   and one test in `roles.test.ts` in the style of its neighbours.
5. `web/src/test/turn-stream.ts`: `turnStream(events: {event: string; data: unknown}[], options?: {delayMs?: number; hold?: Promise<void>}): Response`. It builds an `HttpResponse` with `Content-Type: text/event-stream` whose `ReadableStream` enqueues `": open\n\n"` first, then `event: <name>\ndata: <JSON>\n\n` per event (after `delayMs` each when set), waits for `hold` before closing when given, then closes. It never errors the stream (facts § 8: cancelling a live body can make undici assert).
6. `web/src/mocks/assistant.ts`: `assistantHandlers()` for the dev worker; its turn handler is: `http.post("*/api/v1/projects/:project/assistant/conversations/:id/turns", ...)` answering a scripted turn over the example data: `start`, three `text` deltas, a `tool_call` `search_traces` with `{"from":"2026-01-01T00:00:00Z","to":"2026-01-02T00:00:00Z","filter":"status = 'error'"}`, its `tool_result` (`ok`, `size_bytes` 812, `rows` 3), a `tool_call` `draft_filter`, its `tool_result`, a `draft` `filter` `{"view":"traces","filter":"status = 'error'","range":"24h","include_eval":false,"include_experiments":false}`, text naming the three traces, and `done`. Use 120 ms between events so the screenshot shows streaming.    The same module also exports the handlers the dev worker needs for a new chat, kept in one module-level state (reset by a `resetAssistantMock()` export):
   - `POST …/assistant/conversations` → 201 with a conversation of fixed id `01900000-0000-7000-8000-000000001a02`, title "", `message_count` 0;
   - `GET …/assistant/conversations/01900000-0000-7000-8000-000000001a02` → that conversation with the messages of the scripted turn once a turn has run (user message with its context; assistant message with the `search_traces` call; tool message with its result; assistant message with the `draft_filter` call; tool message with its result and the filter draft part; assistant text), else no messages;
   - `GET …/assistant/conversations` → the fixture list with the new conversation first once it exists;
   - `PATCH`, `DELETE` and `PUT …/messages/{seq}/feedback` → 200/204 echoes.
   They are listed in one exported array `assistantHandlers()`. They are not installed yet (AS4.7 installs them in `browser.ts`, before the manifest handlers so they win).

**Tests:**
- `event-stream.test.ts`: two blocks in one chunk; a block split across three chunks; `\r\n` line ends; multi-line `data`; comment and `id:` lines ignored; a 409 Problem throws `StreamRequestError` with `status` 409 and the parsed `problem`; a 401 calls the unauthorized handler (use `setUnauthorizedHandler` as `session.test.ts` does); abort mid-stream rejects with `AbortError` and calls `onBlock` no more (the stream is held by `hold`; abort, assert, then resolve `hold` so the body closes normally). Serve answers with `server.use(http.post(..., () => turnStream(...)))`.
- `types.test.ts`: each event parses; `ping`, an unknown name, bad JSON and a `tool_result` without `summary` give `null`; `parseDraft` accepts each fixture shape of § Contracts and rejects a filter draft with `view: "logs"`, an evaluator draft with `kind: "webhook"`, a rule draft with a string `sample_rate`.
- `roles.test.ts`: `canUseAssistant`.

**Done when:** at the milestone check.

**Commit:** `web: assistant types, event-stream client and queries (AS M4)`

---

### AS4.2 Org setting page

**Goal:** Organization settings → Assistant: read for every org member, editable by org admins and owners; saving with the switch on runs the server's tool-calling test.

**Edit:** see the phase table.

**Read:** facts § 6, § 8, § 11; `web/src/routes/$org/settings/general.tsx` and `llm-connections.tsx` (and their tests); `web/src/components/settings/settings-page.tsx`, `settings-card.tsx`, `settings-nav.ts`, `settings-home.tsx`, `connection-test.tsx`; `web/src/mocks/writes.ts`; spec 09 § Assistant (PUT and test rows).

**Steps:**
1. `connection-test.tsx`: export the result display (the "Connection works (…)" / "Test failed" lines with the class hint) as `ConnectionTestResult({result}: {result: Schema<"LlmConnectionTestResult">})`, used by its existing form unchanged in behaviour.
2. Route `web/src/routes/$org/settings/assistant.tsx` (`createFileRoute("/$org/settings/assistant")`), loader `ensureQueryData` for `orgAssistantQuery(org)` and the connections list (`connectionsInit(org)`), as `general.tsx` does. Page `SettingsPage title="Assistant" description="The in-app assistant answers questions about this organization's data with one of its LLM connections."`, one `SettingsFormCard`:
   - a switch "Turn on the assistant";
   - a connection `Select` listing the org's connections by name and provider label (`LLM_ADAPTER_LABELS`); empty list: text "Add an LLM connection first." with a link to `/$org/settings/llm-connections`;
   - a model `TextField`, prefilled with the selected connection's `default_model` when the model is empty and the connection changes;
   - the notice under the switch: "The assistant reads traces, prompts, completions and attributes to answer. What it reads is sent to the connection's provider. Turning it on runs a test call that must return a tool call.";
   - a "Test" button that `POST`s `/orgs/{org}/assistant/test` with `{connection_id, model}` and shows `ConnectionTestResult`; 429 shows "Too many tests. Wait a minute.";
   - Save `PUT`s `{enabled, connection_id, model}` (both null when the switch is off and the fields are empty). Field errors from `fieldErrors(error)` go under their fields; the `model` error holds the failed test's detail. On success invalidate `orgAssistantKey(org)` and every `assistantStatusKey` (`queryClient.invalidateQueries({queryKey: ["get", "/api/v1/projects/{project}/assistant"]})`).
   - Non-admins (`!canManageConnections(role)`): the card is read-only with `disabledReason="Only admins and owners can change the assistant."`.
   - `updated_at` shows as "Last changed <relative time>" when present (use the helper the other settings pages use for times).
   - Wrap with `UnsavedChangesGuard` as `general.tsx` does.
3. `settings-nav.ts`: an `orgSettingsNav` entry `{to: "/$org/settings/assistant", label: "Assistant", icon: Bot, description: "Turn on the in-app assistant and pick its model.", keywords: ["assistant", "ai", "chat", "copilot"]}` after LLM connections.
4. `settings-home.tsx`: the Assistant card's status: "On · <connection name> · <model>" when `enabled` and the connection exists in the list, "Off" otherwise.
5. `llm-connections.tsx`: the delete dialog text becomes "Evaluators that use {name} stop working. If the assistant uses it, the assistant turns off." After a successful delete, also invalidate `orgAssistantKey(org)` and the assistant status queries (as step 2).
6. Mocks: in `writes.ts`, add `put_org_assistant` (`PUT /api/v1/orgs/{org}/assistant`, answer: the body with `updated_at` set, or a 400 problem with field `connection_id` "required when enabled" when `enabled` and `connection_id` is null) and `post_org_assistant_test` (`POST /api/v1/orgs/{org}/assistant/test`, answer `static/llm-connection-test.json` as `post_llm_connection_test` does) to `WRITE_OPERATIONS` and `RESPONSES`. In `writes.test.ts` set `LATER_TRACKS` back to `[]` (delete the comment) and add a test for each new answer in the file's style.
7. Run `pnpm --dir web gen:routes` and commit `web/src/routeTree.gen.ts`.

**Tests** (`assistant.test.tsx`, with `renderApp("/acme/settings/assistant")` and `server.use`):
- `shows_the_setting_read_only_to_members`: `me` with role member: controls disabled, the reason shown.
- `turns_it_on`: admin, pick the example connection, type a model, Save: the recorded PUT body is `{enabled: true, connection_id, model}`; the status query is invalidated.
- `shows_a_failed_test_under_the_model`: PUT answers 400 with field `model`: the detail is shown under Model; nothing else changes.
- `runs_the_test`: Test answers `ok: false` with class `permanent`: "Test failed" and the hint show.
- `no_connections_links_to_add_one`.
- `llm-connections.test.tsx`: the delete dialog text names the assistant (update the existing assertion if there is one).

**Commit:** `web: the org assistant setting page (AS M4)`

---

### AS4.3 Panel shell

**Goal:** the panel exists: opened from the top bar or `Ctrl/Cmd+I`, resizable, mounted above the settings switch, showing why it is unavailable or the conversation list; no turns yet.

**Edit:** see the phase table.

**Read:** facts § 1, § 3; `web/src/routes/$org/$project/route.tsx`; `web/src/components/shell/top-bar.tsx`, `project-sidebar.tsx` (`useNarrowViewport`, `NARROW_WIDTH`, line ~60–80); `web/src/components/trace/split-pane.tsx` (resizable use and size persistence); `web/src/components/ui/resizable.tsx`, `sheet.tsx`; `web/src/lib/list-layout.ts` (store style); `web/src/lib/shortcuts.ts`, `shortcuts.test.ts`, `web/src/components/command/palette.tsx`; `useNarrowViewport`.

**Steps:**
0. Move `useNarrowViewport` (and `isNarrowWindow`, `NARROW_WIDTH`) from `components/shell/project-sidebar.tsx` into `web/src/hooks/use-narrow-viewport.ts`, exported, unchanged in behaviour (window narrower than 1024 px, updated on resize). `project-sidebar.tsx` imports it from there. Grep for any other user of `NARROW_WIDTH` and update its import.
1. `lib/assistant/panel.ts`: § Contracts › Panel store.
2. Shortcut: in `shortcuts.ts` add `"assistant"` at the end of the `ShortcutId` union and a `SHORTCUTS` row right after the `⌘K` row: `{keys: "⌘I", description: "Open or close the assistant", scope: "project", bindings: [{id: "assistant", key: "i", mod: true}]}` (the `⌘K` row's `keys` is `"⌘K"`). In `reduceShortcut`, after the palette check: `key.key.toLowerCase() === "i" && (key.metaKey || key.ctrlKey) && !key.altKey && !key.shiftKey` → `{chord: null, action: "assistant"}` (before the typing check, so it works in the composer). In `palette.tsx` `onKeyDown`: `if (action === "assistant") { event.preventDefault(); dispatchShortcut("assistant", event); return; }`. Panel components subscribe with `useShortcut("assistant", togglePanel)` in the project layout only, so the shortcut does nothing outside a project.
3. `assistant-button.tsx`: `AssistantButton({org, project})`: reads the project's role from `$api.useQuery("get", "/api/v1/projects/{project}", projectInit(org, project))` (cached by the project loader); renders nothing unless `canUseAssistant(role)`; a ghost icon button (`Sparkles`), `aria-label="Assistant"`, `aria-pressed={open}`, tooltip "Assistant (⌘I)", toggles the panel. In `top-bar.tsx`, render it before `<IngestDot/>` when `project` is set.
4. `assistant-panel.tsx`: `AssistantPanel()` (inside `ProjectProvider`):
   - Header: the conversation menu (step 5), a "New chat" icon button (`SquarePen`, `aria-label="New chat"`), a close button (`X`, `aria-label="Close the assistant"`).
   - Body: `useQuery(assistantStatusQuery(org, project))`. While loading: skeleton. `usable: false`: `<Unavailable reason=… />`. Usable: the transcript area (AS4.4 fills it; here an empty state "Ask about this project's traces, metrics, evaluations and settings.") and, under it, the connection line "Answers use **{connection_name}** ({adapter label}, {model}). What the assistant reads is sent to that provider."
   - `Esc`: the panel's root `onKeyDown` handles `event.key === "Escape"` by calling `(document.activeElement as HTMLElement | null)?.blur()` and `event.stopPropagation()`; it does not close the panel (the palette's blur behaviour, D-AS-27).
   - The root is wrapped in `TooltipProvider` (the panel sits outside `ProjectSidebar`, which provides the one the pages use).
5. `conversation-menu.tsx`: a `DropdownMenu` whose trigger shows the current title ("New chat" when empty) and lists `conversationsQuery` (newest first, title or "Untitled", relative time). Each item selects (`setCurrentConversation`). A "Rename" item opens a small dialog with a `TextField` (1–80 characters) that `PATCH`es and invalidates both keys. A "Delete" item opens an `AlertDialog` ("Delete this chat? This cannot be undone.") that `DELETE`s, clears the current id when it was current, and invalidates. "New chat" `POST`s `…/conversations`, sets the current id, invalidates the list. When the current id is no longer in the list (deleted or expired), clear it.
6. `unavailable.tsx`: `Unavailable({reason})`: `forbidden` → "The assistant is for members, admins and owners of this project."; `disabled` → "The assistant is off in this organization." plus, for org admins (`canManageConnections` from `me`), a link "Turn it on" to `/$org/settings/assistant`; `no_connection` → "The assistant's LLM connection was deleted." plus the same admin link; `restore_mode` → "The assistant is paused while this server is in restore mode."
7. Layout (`routes/$org/$project/route.tsx` `ProjectLayout`). The page must never remount when the panel opens or closes, so the element tree above the `inSettings` ternary is the same in every state:
   - Follow `components/trace/split-pane.tsx` (lines ~137, ~200–275): both panels are always mounted and the group's layout is set through `useGroupRef()` and `groupRef.current?.setLayout(...)`. `react-resizable-panels` 4.21 has no `order` prop; children order is DOM order; numeric sizes are pixels and strings are percents, so every size below is a percent string.
   - Always render `<ResizablePanelGroup id="assistant-layout" groupRef={groupRef} orientation="horizontal" disabled={!wide} className="min-h-0 flex-1" defaultLayout={{page: 100, assistant: 0}} onLayoutChanged={(layout, meta) => { if (wide && meta.isUserInteraction) writePanelSize(layout.assistant); }}>`, where `wide = open && canUseAssistant(data.role) && !narrow` (`narrow` from `useNarrowViewport()`). Its parent is the `flex min-h-0 flex-1 flex-col` div of `top-bar-layout.tsx`.
   - First child: `<ResizablePanel id="page" minSize={wide ? "50%" : "100%"} className="relative" style={{overflow: "hidden"}}>` holding the existing ternary, unchanged.
   - Then `{wide ? <ResizableHandle withHandle /> : null}`.
   - Second child: `<ResizablePanel id="assistant" minSize={wide ? "20%" : "0%"} maxSize={wide ? "50%" : "0%"} style={{overflow: "hidden"}}>` holding `{wide ? <AssistantPanel/> : null}`.
   - An effect on `wide` calls `groupRef.current?.setLayout(wide ? {page: 100 - readPanelSize(), assistant: readPanelSize()} : {page: 100, assistant: 0})`, as `split-pane.tsx` does at ~202.
   - When `open && canUseAssistant(data.role) && narrow`: a `Sheet` (`side="right"`, class `w-full sm:max-w-none`) holding `<AssistantPanel/>`, `open` bound to the store, `onOpenChange(false)` closing it.
   - The project sidebar inside the first panel is positioned `absolute` within its own wrapper (`components/ui/sidebar.tsx:~219`), so it stays inside the first panel; the first panel's content keeps `h-[calc(100dvh-40px)]` from `SidebarProvider`.
   - `<AssistantPanel/>` is keyed by `projectRef(org, project)`, so switching projects resets its local state.
8. `useShortcut("assistant", togglePanel)` in `ProjectLayout`, guarded by `canUseAssistant(data.role)`.

**Tests** (`assistant-panel.test.tsx`; `shortcuts.test.ts`). The manifest serves only conversation `…1a01`, and `src/test/setup.ts` fails a test on any unhandled request, so each test that creates, renames, deletes or rates installs `server.use` handlers for those routes (`POST …/conversations` returning a new id, `GET …/conversations/{new id}`, the list with it, `PATCH`/`DELETE`/`PUT …/feedback`); none exist in `writes.ts`.
- `reduceShortcut` returns `assistant` for Ctrl+I and Meta+I, also while typing; not for Alt+Ctrl+I or Shift+Ctrl+I; the help rows include the new row.
- `opening_the_panel_keeps_the_page`: on `/acme/support-bot/evaluators/new`, type a name in the form, open then close the panel: the typed name is still there.
- `the_top_bar_button_opens_the_panel`: `renderApp("/acme/support-bot/traces")`, click "Assistant": the panel shows the connection line from `project-assistant.json`.
- `ctrl_i_toggles_the_panel` and `the_panel_stays_open_in_project_settings` (navigate to a project settings page: still visible).
- `viewers_have_no_button` (override `GET /projects/{project}` with role viewer).
- one test per unavailable reason, via `server.use` on the status route; the admin link shows only for an org admin.
- `new_chat_creates_and_selects` (records the POST), `rename_and_delete` (records PATCH and DELETE bodies and paths).
- `panel.test.ts`: size clamped to [20, 50]; open state and current conversation persist; storage that throws falls back to memory.

**Commit:** `web: the assistant panel shell (AS M4)`

---

### AS4.4 Turns in the panel

**Goal:** the user sends a message with the page context and sees the answer stream: Markdown text, collapsed tool steps, draft placeholders (cards come in AS4.5), Stop, thumbs, and errors.

**Edit:** see the phase table.

**Read:** facts § 0, § 4, § 5 (search types), § 7; `web/src/components/payload/markdown.tsx`; `web/src/lib/search/*.ts`; `web/src/components/command/recent-traces.ts` (`openTraceId`); `components/shell/page-crumb.ts`; spec 09 § Assistant (event order, statuses, error classes); the fixture conversation `fixtures/api/responses/assistant-conversations/01900000-0000-7000-8000-000000001a01.json`.

**Steps:**
1. `lib/assistant/context.ts`: § Contracts › Page context. Parse the pathname after `/<org>/<project>` with plain string and regex checks (no router calls), reading search values only through the existing `parse*` helpers of `lib/search/common.ts` so invalid values are dropped.
2. `lib/assistant/transcript.ts`: the view model, pure functions:
   ```ts
   export type Step = { id: string; name: string; arguments: unknown; result?: { ok: boolean; size_bytes: number; rows?: number; error_code?: string; stubbed?: boolean } };
   export type Block =
     | { type: "text"; text: string }
     | { type: "step"; step: Step }
     | { type: "draft"; id: string; kind: DraftKind; draft: unknown };
   export type Bubble =
     | { role: "user"; seq: number; text: string; context?: Record<string, unknown> }
     | { role: "assistant"; seqs: number[]; blocks: Block[]; status: "complete" | "cancelled" | "failed" | "streaming"; feedback?: "up" | "down"; feedbackSeq?: number; traceId?: string };
   /** Builds bubbles from stored messages: consecutive assistant and tool messages of one turn form one assistant bubble; a tool_result fills its step; a draft part becomes a draft block after its step. `feedback`, `feedbackSeq` (its `seq`) and `traceId` (its `turn_trace_id`) come from the bubble's last assistant message. */
   export function bubblesOf(messages: AssistantMessage[]): Bubble[];
   /** Applies one live event to the streaming bubble (the last one). */
   export function applyEvent(bubble: Bubble & { role: "assistant" }, event: TurnEvent): Bubble & { role: "assistant" };
   ```
   A `text` part or delta appends to the last block when it is text, else opens a text block. A `tool_call` adds a step. A `tool_result` fills the step with the same `id`. `draft` adds a draft block. `done` sets `status: "complete"`. `error` sets `status: "failed"`.
3. `answer-markdown.tsx`: `AnswerMarkdown({text, org, project})`: render through the same plugins and components map as `payload/markdown.tsx` (export its module-private `components` constant as `markdownComponents`, and nothing else; `Markdown`'s behaviour does not change), with `disallowedElements={["img"]}` and `unwrapDisallowed`, and an `a` override:
   - an href starting with `/` and not `//`: a TanStack `Link` with `to={href}` (same tab);
   - any other `http:`/`https:`/`mailto:` href: the link text, then ` (` + the full URL in `text-muted-foreground` + `)`, `target="_blank" rel="noopener noreferrer"`;
   - anything else: the text only.
4. `tool-step.tsx`: a native `<details>` (closed by default) whose summary shows a status icon, the tool's label (`search_traces` → "Searched traces"; a table of labels for the 24 tool names from `crates/server/src/assistant/tools/mod.rs` `specs()`, unknown names shown as the raw name), and the result summary ("3 rows", "812 bytes", "failed: <error_code>", "result expired" when `stubbed`). Inside: the arguments as formatted JSON in `CodeBlock`; for `query_sql` the `query` argument as SQL in `CodeBlock`; a "Open in SQL console" link is added in AS4.5 with the hand-off.
5. `composer.tsx`: a `Textarea` (`aria-label="Message the assistant"`, max 16,384 bytes counted with `TextEncoder`, a counter when over 15,000), Enter sends, Shift+Enter adds a line, a Send button (`aria-label="Send"`) and, while a turn streams, a Stop button instead (`aria-label="Stop"`). Above it the chips: one per context field (`key: value`, values cut to 40 characters in display), each with a remove button (`aria-label="Remove {key}"`); the `page` chip is not removable. The chips follow the page: they are recomputed from the router location and the SQL tabs (`readTabs(ref)` + the active tab) whenever the location changes, and a removed field stays removed until the page kind changes.
6. `transcript.tsx`: renders bubbles: user bubbles as plain text with their context shown as small chips; assistant bubbles as blocks in order (text with `AnswerMarkdown`, steps with `ToolStep`, drafts as a placeholder `<div data-draft-id>` that AS4.5 replaces); a "Stopped" note for `cancelled`, "This answer failed." for `failed`; thumbs up/down buttons (`aria-pressed`) on complete assistant bubbles that `PUT …/messages/{feedbackSeq}/feedback` with `{feedback}` (clicking the pressed one sends `null`) and invalidate the conversation; a "View trace" link to `/$org/$project/traces/{traceId}` when the bubble has a trace id. It scrolls to the bottom on new content unless the user scrolled up.
7. Sending, in `assistant-panel.tsx`:
   - With no current conversation, `POST …/conversations` first and select it.
   - Append an optimistic user bubble and an empty streaming assistant bubble; call `postEventStream(turnUrl(...), {message, context: fitContext(chips)}, {signal, onBlock})` with an `AbortController` kept in a ref; feed each `parseTurnEvent` result to `applyEvent`.
   - When the promise settles (done, error, abort or failure), invalidate `conversationKey` and `conversationsKey`, so the stored messages replace the live bubbles.
   - Stop aborts the controller; the bubble shows "Stopped".
   - `StreamRequestError`: 409 shows the problem's `detail` ("a turn is already running in this conversation", the room limit's detail, "the assistant is off in this organization"); 429 shows "Too many questions. Try again in {retryAfter} seconds." when `retryAfter` is set, else "Try again in a minute."; 503 shows the restore-mode text of AS4.3; 400 shows the field error under the composer; status 0 shows "The connection to the server was lost.".
   - An `error` event shows its message with a hint per class: `retryable` → a "Retry" button that resends the same message and context; `pause` → "Ask an admin to check the LLM connection."; `permanent` → nothing more.
   - Switching conversations, closing the panel or leaving the project aborts a running turn.

**Tests** (`turn.test.tsx`, `context.test.ts`, `transcript.test.ts`). Turn tests start with the current conversation set to `…1a01` (`setCurrentConversation`), serve the turn with `server.use(http.post(…/turns, () => turnStream(...)))`, and override `GET …/conversations/01900000-0000-7000-8000-000000001a01` with a mutable response that returns the fixture's messages plus the turn's stored messages once the turn has run, so the refetch after the turn shows the same answer.
- `context.test.ts`: one case per row of the page-context table (path + search → object); list default range `24h`; `withoutField` keeps `page`; `fitContext` on a 20 KB SQL query gives ≤ 8,192 bytes with `truncated: true`.
- `transcript.test.ts`: `bubblesOf` on the fixture conversation (one user bubble, one assistant bubble with a step and text); a stubbed result; `applyEvent` for each event kind; a draft block lands after its step.
- `turn.test.tsx`:
  - `streams_an_answer`: open the panel on `/acme/support-bot/traces?filter=status%20%3D%20%27error%27`, type, send; the recorded turn body has `context.page === "traces"` and the filter; the answer text and a "Searched traces" step appear; the conversation is refetched at the end.
  - `removed_chip_is_not_sent`.
  - `stop_aborts`: a held stream (`hold`), click Stop: the request's signal is aborted and "Stopped" shows.
  - `error_event_shows_retry`, `busy_409_shows_detail`, `rate_limited_429`.
  - `thumbs_up_sends_feedback` (records the PUT path with the seq and the body).
  - `links_and_images`: an answer with `![x](https://evil.example/x.png)` renders no `img`; `[trace a1](/acme/support-bot/traces/…)` is an in-app link; `[site](https://example.com)` shows `https://example.com` and has `rel="noopener noreferrer"`.

**Commit:** `web: assistant turns in the panel (AS M4)`

---

### AS4.5 Drafts and Apply

**Goal:** draft cards show what each draft holds, and Apply opens the target prefilled without saving or overwriting anything (D-AS-6, D-AS-28).

**Edit:** see the phase table.

**Read:** facts § 5; `web/src/lib/search/traces.ts`, `spans.ts`, `sessions.ts`, `users.ts`, `common.ts`; `web/src/lib/time-range.ts`; `web/src/lib/sql/tabs.ts`; `web/src/components/dashboard/dashboard-header.tsx` (`DashboardSwitcher` owns `creating` and renders `NewDashboardDialog`), `dashboard-body.tsx` (`DashboardGrid`, `Session`, `startEditing`, the `[active]` effect at ~145, `canEdit` at ~159), `dashboard-dialogs.tsx` (`NewDashboardDialog`, `NewForm`); `web/src/routes/$org/$project/route.tsx` `KeptDashboard` (a hidden copy stays mounted); `web/src/components/evaluators/evaluator-form.tsx` (`initialValues`, `pickTemplate`); `web/src/components/online-rules/rule-form.tsx` (`initialValues`, `percentText`); the two `new.tsx` routes.

**Steps:**
1. `lib/assistant/handoff.ts`: § Contracts › Hand-off store.
2. `lib/assistant/apply.ts`:
   ```ts
   /** The list route and search a filter draft opens. */
   export function filterTarget(draft: FilterDraft): { to: "/$org/$project/traces" | "/$org/$project/spans" | "/$org/$project/sessions" | "/$org/$project/users"; search: Record<string, string | number> };
   /** The SQL tab draft of a SQL draft: params as strings (JSON.stringify for non-strings). */
   export function sqlTabDraft(draft: SqlDraft): import("@/lib/sql/draft").SqlDraft;
   ```
   `filterTarget`: `filter`, `search` (traces and spans only; dropped for sessions and users), `range`, or `from`/`to` converted to RFC 3339 (`new Date(Number(x)).toISOString()` when the value is all digits, else as given), `eval: 1` when `include_eval`, `exp: 1` when `include_experiments`.
3. `draft-card.tsx`: `DraftCard({kind, draft})`: `parseDraft`; `null` → "This draft could not be read." Otherwise a bordered card with a title per kind and its content:
   - filter: the view, the DSL in `CodeBlock`, the range, the flags; a note "Search is not available on {view}; it is left out." when `search` is set on sessions or users;
   - sql: the query in `CodeBlock` (SQL), the params, the expected columns;
   - dashboard: name, "New dashboard" or "Changes to this dashboard", and the widget titles in order;
   - evaluator: name, kind, for a judge the connection name (from the org connections list, `connectionsInit(org)`), model and the full prompt template in `CodeBlock`, the output score; for a check its check name and params;
   - online rule: name, evaluator name (from `evaluatorQuery(org, project, evaluator_id)` in `api/queries-evaluation.ts`; a 404 shows "Unknown evaluator" and disables Apply with that reason), target, filter in `CodeBlock`, the full variable mapping table, sample rate as a percent, budget.
   An "Apply" button per card (label per kind: "Open in traces", "Open in SQL console", "Open in dashboard editor", "Open evaluator form", "Open rule form").
4. Apply actions (in `draft-card.tsx`):
   - filter: `navigate({to, params: {org, project}, search})`.
   - sql: `handDraft(ref, {kind: "sql", draft})`, then `navigate({to: "/$org/$project/sql", params, search: range/from/to as for filters})`.
   - dashboard: `handDraft(ref, {kind: "dashboard", draft})`, then navigate to `/$org/$project/dashboard/$dashboardId` for an edit draft, else `/$org/$project/dashboard`.
   - evaluator: `handDraft`, then navigate to `/$org/$project/evaluators/new`.
   - online rule: `handDraft`, then navigate to `/$org/$project/online-rules/new`.
5. SQL console (`lib/sql/tabs.ts` `useSqlTabs`): add an effect that, on mount and on each `subscribeDrafts(projectRef, …)` call, runs `const draft = takeDraft(projectRef, "sql"); if (draft) add(sqlTabDraft(draft));` (import `sqlTabDraft` from `lib/assistant/apply.ts`). The new tab becomes active (as `addTab` already does); existing tabs are untouched. `addTab` returns the state unchanged at `MAX_TABS` (20), so the SQL card's Apply (and the tool step's "Open in SQL console") is disabled with the reason "Close a SQL tab first: 20 are open." when `readTabs(ref).tabs.length >= MAX_TABS`. `lib/sql/tabs.ts` imports a type `SqlDraft` from `@/lib/sql/draft`; import the assistant's as `type SqlDraft as AssistantSqlDraft` where both are in scope. `sqlTabDraft` returns a value of `lib/sql/draft.ts`'s `SqlDraft` type (read it for any further required field and set it).
6. Dashboards:
   - `dashboard-dialogs.tsx` `NewDashboardDialog` gets an optional `initial?: {name: string; description?: string; definition: Definition}`; when given, `NewForm` starts with that name and description, hides the template picker, and creates with `initial.definition`.
   - New-dashboard drafts (no `dashboard_id`): `DashboardSwitcher` in `dashboard-header.tsx` (line ~56; it owns `creating` at ~63 and renders `NewDashboardDialog`) is mounted only for the visible dashboard (`DashboardBody` renders it inside `{active ? <TopBarCrumb>…</TopBarCrumb> : null}`, `dashboard-body.tsx:~382`), so it needs no `active` flag. An effect, run on mount and from a `subscribeDrafts(ref, …)` subscription, does `const d = peekDraft(ref, "dashboard"); if (d && d.dashboard_id === undefined && canManage) { takeDraft(ref, "dashboard"); setInitial(d); setCreating(true); }`, where `canManage` is the switcher's own (`local || canManageDashboards(data.role)`). It passes `initial` to its `NewDashboardDialog` and clears it when the dialog closes.
   - Edit drafts (`dashboard_id` set), in `DashboardGrid` (`dashboard-body.tsx`):
     - Extract the session object `startEditing()` builds (the argument of its `setSession({...})`) into a function `editSession(...)` used by `startEditing`.
     - Merge the existing `[active]` reset effect (~145) with the take, in one effect with deps `[active, canEdit, custom?.id]` placed after `canEdit` is declared (~159): first, when `active` changed to true, the existing reset (`setSession(null); setOpen(null)`) as today; then `takeEditDraft()`, which does `const d = peekDraft(ref, "dashboard"); if (active && canEdit && custom && d?.dashboard_id === custom.id) { takeDraft(ref, "dashboard"); setSession({...editSession(...), draft: d.definition, version: d.version}); }`.
     - A `subscribeDrafts(ref, takeEditDraft)` subscription (in an effect with the same deps) handles a draft handed while the dashboard is already open, the usual case.
     - A grid that is hidden, or where `canEdit` is false, leaves the draft in the store. Saving goes through the existing `persist(draft, version)`, so a moved version gets the existing stale message.
   - The dashboard card's Apply is disabled with a `DisabledReason` when `!canManageDashboards(role)` ("Only members, admins and owners edit dashboards.") or when the window is narrower than 768 px (`useIsMobile()`, the editor's own limit: "Dashboards are edited on wider screens.").
7. Evaluator form: `EvaluatorForm` gets `initialDraft?: EvaluatorDraft`. Refactor `initialValues(evaluator)` so its body becomes `valuesOf(source: {name: string; kind: "llm_judge" | "check" | "webhook"; config: Record<string, unknown>; output_score?: Schema<"OutputScoreBody">} | undefined): FormValues`, with `initialValues(evaluator)` calling `valuesOf(evaluator)` and the form calling `valuesOf(initialDraft)` when a draft is given; one mapping, no copy. The form stays in create mode (`saved` false). `evaluators/new.tsx`: `const [draft] = useState(() => takeDraft(ref, "evaluator"));` (a `useState` initializer, not an effect) and pass it; `initialTemplateId` is ignored when a draft is given.
8. Rule form: `RuleForm` gets `initialDraft?: RuleDraft`. Refactor `initialValues(rule)` the same way: its stored-rule branch becomes `valuesOf(source: {name; evaluator_id; target; filter?; variable_mapping; sample_rate; budget_usd?; budget_period?; enabled})` (a stored `Rule` and a draft use the same field names), used by both. The by-id evaluator fetch at `rule-form.tsx:~302-309` (`fetchMissing`, gated on `saved`) also runs for `initialDraft.evaluator_id`, so an evaluator beyond the first page still shows. `online-rules/new.tsx` takes the draft as step 7 does.
9. `tool-step.tsx`: for `query_sql`, an "Open in SQL console" button that hands `{kind: "sql", draft: {query, params: arguments.params ?? {}, columns: []}}` and navigates as step 4.
10. `transcript.tsx`: replace the draft placeholder with `<DraftCard/>`.

**Tests** (`handoff.test.ts`, `apply.test.ts`, `apply.test.tsx`):
- `handoff.test.ts`: take once; replace an untaken one; per-project isolation; listeners called.
- `apply.test.ts`: `filterTarget` for each view; epoch-ms `from`/`to` → ISO; `search` dropped on sessions; flags. `sqlTabDraft` stringifies a number param.
- `apply.test.tsx` (vitest, the web half of the e2e, D-AS-31):
  - `applies_a_filter_draft`: a conversation fixture override whose last tool message holds a filter draft; open the panel, click "Open in traces": the location is `/acme/support-bot/traces?filter=…&range=24h` and the list's filter field shows the filter.
  - `sql_draft_opens_a_new_tab_without_touching_the_open_one`: start on `/sql` with one tab holding user text; apply: two tabs, the old one unchanged, the new one active with the draft query.
  - `evaluator_draft_prefills_the_form`: the name and the prompt template are in the form; no POST or PATCH was recorded (`recordRequests`).
  - `rule_draft_with_unknown_evaluator_cannot_apply`.
  - `new_dashboard_draft_opens_the_dialog_prefilled`; `edit_dashboard_draft_opens_the_editor`.

**Commit:** `web: assistant draft cards and Apply (AS M4)`

---

### AS4.6 Rust e2e

**Goal:** `just e2e` runs one assistant turn against the mock provider over ingested data and checks the answer, the filter draft, the stored conversation and the turn trace.

**Edit:** `crates/cli/tests/e2e.rs` only.

**Read:** facts § 10; `crates/cli/tests/e2e.rs` (`start`, `Server`, `Api`, `read_until`, `playground_run_end_to_end`, the OTLP ingest helpers used by `live_streams_and_every_read_route_work_end_to_end`); `crates/server/src/routes/assistant.rs` tests `ping_turn()` and `text_turn()` (mock turn shapes); `tools/capture/src/mock.rs` (`Mock`, `Turn`).

**Steps:**
1. Add `#[tokio::test(flavor = "multi_thread")] async fn assistant_turn_end_to_end()` after `playground_run_end_to_end` (`e2e.rs:~1315`), in its style: same mock serving on `127.0.0.1:0`, same extra config `"\n[security]\nallow_private_network_egress = true\n"`, same connection creation with `base_url: http://{addr}/v1`, same teardown as its last lines (`~1435-1438`).
2. Mock turns, in order (shapes as `crates/server/src/routes/assistant.rs` tests `ping_turn()` and the `assistant/turn.rs` tests' `calls_turn`/`text_turn`):
   1. a `ping` tool call (consumed by the setting test);
   2. a `search_traces` tool call with arguments `{"from": <RFC 3339 of start_ns minus 1 h>, "to": <RFC 3339 of now plus 1 h>, "filter": "status = 'error'"}`;
   3. a `draft_filter` tool call `{"view": "traces", "filter": "status = 'error'", "range": "24h"}`;
   4. a text answer "There are errors; open the draft to see them." with `finish: "stop"`.
   Build the mock after computing `start_ns`, so the arguments hold the times.
3. Ingest: add a helper `timed_error_request(trace: &str, start_ns: i64)` next to `timed_request` (`e2e.rs:1064`), copying its body but with `fixture_request("sdk-observe-error", trace)` (`fixtures/otlp/cetra/sdk-observe-error/`, which has an error span). Use `trace = format!("{:032x}", 41)` and `start_ns = now_ns() - 60_000_000_000`. Post it with `post_otlp_http` exactly as the existing call at `e2e.rs:~1118-1130` does (same URL, key, content type and encoding), then `rows_until(&server.api, &format!("{project}/traces"), 1, Duration::from_secs(10))` where `project` is `/api/v1/projects/{server.proto_id}` as the other tests build it; assert the bool.
4. Create the connection; add `Api::put(path, body)` to the `Api` struct (`e2e.rs:28-88`) mirroring `post` (cookie and `origin: ORIGIN` headers), and `PUT /api/v1/orgs/acme/assistant {enabled: true, connection_id, model: "gpt-test"}` → 200; `GET {project}/assistant` → `usable: true`.
5. `POST {project}/assistant/conversations` → 201; `POST {project}/assistant/conversations/{id}/turns` with `{"message": "Which traces failed?", "context": {"page": "traces"}}`, started as the playground test starts its run (`e2e.rs:~1360-1380`: `server.client.post(...).header("cookie", ...).header("origin", ORIGIN).json(&body).send()`), then `read_until(&mut response, "event: done", Duration::from_secs(30))`. Assert that the text contains, in this order: `event: start`, `event: tool_call` with `search_traces`, `event: tool_result` with `"ok":true`, `event: draft` with `"kind":"filter"`, `event: text`, `event: done`.
6. `GET {project}/assistant/conversations/{id}`: a tool message holds a `draft` part of kind `filter` whose `draft.filter` is `status = 'error'`, and the last message is an assistant message with a text part.
7. "Apply" over HTTP: `GET {project}/traces?filter=status%20%3D%20%27error%27&range=24h` → 200 with at least one row.
8. The turn trace: `rows_until(&server.api, &format!("{project}/traces?include_eval=true&filter=environment%20%3D%20%27cetra-assistant%27"), 1, Duration::from_secs(10))`; assert the row's `environment` is `cetra-assistant`, as the playground test asserts `cetra-playground`.
9. `mock.finish()`: no errors and `log.served.len() == 4` (the ping test plus three model calls of the turn: `search_traces`, `draft_filter`, the text).

**Tests:** the test above.

**Commit:** `e2e: an assistant turn over ingested data (AS M4)`

---

### AS4.7 Docs and screenshots

**Goal:** the Assistant docs page, edits to the related pages, and the screenshot entries (the PNGs are written at the milestone check).

**Edit:** see the phase table.

**Read:** `website/STYLE.md`; facts § 9; `website/src/content/docs/docs/platform/mcp.mdx` (a neighbour page); `website/screenshots.json` and `website/scripts/screenshots.mjs`; `web/src/mocks/browser.ts`.

**Steps:**
1. `platform/assistant.mdx` (title "Assistant", description ≤ 160 characters): what it is; turning it on (numbered steps: Organization settings → Assistant, pick a connection and a model, Save runs a test); who can use it (members and above, not API keys); asking (the panel, `Ctrl/Cmd+I`, context chips); drafts and Apply (nothing is saved until you save); what is stored and for how long (conversations private to you, deleted after 30 days idle; tool results reduced to a summary after about 25 hours; each turn traced in `cetra-assistant` with no message text, hidden by default, not counted as units); what is sent to the provider; limits (D-AS-15 numbers, 200 messages per chat, 100 chats per project); how it differs from the MCP server. Two images: `assistant-panel.png` and `assistant-setting.png`, imported as the neighbours do. Use "chat" for a conversation in UI wording.
2. `astro.config.mjs`: add the page to the Platform sidebar after MCP server.
3. Short edits: `evaluation/llm-connections.mdx` (the assistant uses one connection; deleting it turns the assistant off), `tracing/privacy-controls.mdx` (what the assistant stores and sends; link to the page), `platform/mcp.mdx` (one line pointing to the assistant), `get-started/ui-tour.mdx` (one line about the top-bar Assistant button).
4. `web/src/mocks/browser.ts`: install `...assistantHandlers()` (AS4.1) before `writeHandlers` and the manifest `handlers`.
5. `website/screenshots.json`: entries `assistant-panel` (path `/acme/support-bot/traces`, actions: click the top-bar `button[aria-label="Assistant"]`, click `textarea[aria-label="Message the assistant"]` and type "Which traces failed in the last day?", click `button[aria-label="Send"]`; `waitAfter` "Open in traces") and `assistant-setting` (path `/acme/settings/assistant`, `waitFor` "Turn on the assistant"), in the file's format.

**Commit:** `docs: the assistant page (AS M4)`

---

### AS4.8 Closeout

**Goal:** the track's records are complete.

**Edit:** see the phase table.

**Steps:**
1. `CHANGELOG.md` § `[Unreleased]` › `Added`, one bullet each, in the file's style:
   - "In-app assistant (D193, D-AS-1 to D-AS-31): a chat panel in each project (`Ctrl/Cmd+I`) that answers questions over traces, metrics, evaluations and settings with the organization's LLM connection, and drafts filters, SQL, dashboards, evaluators and online rules that you apply."
   - "Organization setting Assistant (D-AS-3, D-AS-24): off until an admin picks a connection and a model; turning it on runs a tool-calling test."
   - "Assistant API (D-AS-9, D-AS-10): conversations, turns streamed over SSE, feedback, and the org setting under `/api/v1/…/assistant`."
2. `docs/spec/11-ui.md`: a § Assistant panel section (layout D-AS-26, contents D-AS-18, shortcut D-AS-27, Apply D-AS-28, page context D-AS-29 with the table of § Contracts › Page context); the shortcut row in § Quality bar's shortcut list; the org settings page list gains Assistant.
3. `00-design.md` and `docs/mvp-decisions.md` § Track AS: D-AS-26 to D-AS-31 rows (as this plan's table), each with its rejected alternative in the design doc.
4. `docs/roadmap.md` AS row: status "**Done** (PR #182)", decisions "D193, D-AS-1–D-AS-31".
5. This file: § Deviations during implementation, listing each phase's reported deviations.

**Commit:** `docs: close track AS (AS M4)`

---

## Milestone check (after AS4.8)

1. `just docs-screenshots assistant-panel assistant-setting` (needs Chromium; `pnpm --dir website exec playwright install chromium` if missing), then commit the two PNGs.
2. `just check`, then `just e2e`. Fix what fails, in one commit `Make the AS M4 checks pass`.
3. A Sonnet review of the M4 diff; fixes in `Fix the AS M4 review findings`.
4. Push to PR #182 and update its body.
