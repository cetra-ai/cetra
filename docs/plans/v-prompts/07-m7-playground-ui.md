# V · M7 — Playground UI and closeout

Implementation plan for milestone M7 of track V. It has 5 phases, **V7.1 to V7.5**, one commit each, done in order. **Haiku** implements each phase with no context beyond this plan, the design doc and the files it names; **Sonnet** reviews the milestone. If this plan and the code disagree, **STOP and report** instead of guessing.

**Checks (the user's rule):** no phase runs pnpm, vitest, tsc, biome, cargo, `just` or any build. Each phase writes code and tests carefully and commits. After V7.5 the milestone runs `pnpm --dir web gen:routes` and `just check` once, and fixes what fails.

**Prerequisites** (each must print at least one line, else STOP): M4 and M6 are in the branch.

```bash
grep -n '"/api/v1/projects/{project}/playground/runs"' web/src/api/schema.gen.ts
grep -n "export function canRunPlayground" web/src/lib/roles.ts
ls web/src/components/prompts/template-editor.tsx web/src/api/queries-prompts.ts web/src/lib/prompts.ts
ls 'web/src/routes/$org/$project/prompts/$prompt.tsx'
```

**Read first** (every phase): `AGENTS.md`; `docs/spec/11-ui.md`; `docs/plans/v-prompts/00-design.md` § UI, D-V-20 to D-V-28, D-V-33 to D-V-37; `crates/server/src/api/playground.rs` and `crates/server/src/routes/playground.rs` (the request fields, the events and their JSON, the 400 field names, the limits); the facts file is not needed. This file up to § V7.1, then your phase.

## What M7 is, and what it is not

M7 delivers:
- the **playground page** `/:org/:project/playground`: messages with variables and placeholders, tools and a response format, 1–4 model columns each with its connection, model and parameters, streamed answers with usage, cost, latency, time to first token and a trace link, cancel, Run all;
- opening it **from a prompt version**, **from an LLM span**, or **from a past run**, with the draft kept in the browser per project;
- **Save as new version** of an existing prompt or a new prompt;
- **Recent runs** (the user's `cetra-playground` traces);
- "Open in playground" on the prompt page and on LLM spans, the sidebar entry and go-key `g l`;
- dev-server mocks, the docs page and screenshots, spec 11, the decisions, the changelog, and the track's closeout (roadmap row **Done**).

M7 does not add dataset runs (design Q1), MCP tools (Q2), or tool execution.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-V-20 | one POST per model column, answered as SSE; aborting the request cancels |
| D-V-21, D-V-37 | the request: canonical messages or placeholder entries, variables, placeholders, params, tools, tool choice, response format, source |
| D-V-24 | 1–4 columns in the UI; 429 for limits |
| D-V-25 | text, tool-call and tool-result parts; media and unknown parts are refused before running |
| D-V-26 | members and above run; viewers see the page read-only |
| D-V-27 | save as a version: text messages only; a message with tool parts blocks the save and is named |
| D-V-28 | the draft lives in the URL (source) and `localStorage` per project; recent runs are traces |

Made precise by this plan (V7.5 records it as **D-V-38** in `docs/mvp-decisions.md` § Track V and the design; check the number is free):
- **Raw fetch for runs.** Runs use `fetch` with `credentials: "same-origin"`, read the body as SSE with a small parser, and call `reportUnauthorized` on a 401; openapi-react-query is not used for the stream.
- **Editor.** A message is edited as a role and its text; a message that holds non-text parts (tool calls or results, from a span or a past run) is edited as its parts' JSON. Variables are listed from the text with the same `{{ name }}` rule as the SDKs (display only; the server validates).
- **Models are free text**, defaulting to the connection's `default_model` (there is no model list endpoint).
- **Go-key `l`** for the playground.

## Versions

No new dependency.

## Conventions for every phase

- `web/` rules as in M4's plan (type scale, accessible names, `Schema<"…">` types, `useProject()`, `DisabledReason`, inline feedback, MSW `server.use` in tests, `renderApp`), plus: every `localStorage` access is in `try/catch` with an in-memory fallback (pattern of `web/src/components/shell/recent-items.ts`); keys start with `cetra.`.
- Never log or `console` message content.
- Commit once per phase with the exact subject, staging your files by path; no attribution lines; no builds or tests (§ Checks).

## Shared reference

### Types (`web/src/lib/playground.ts`, V7.1)

```ts
export type Part =
  | { type: "text"; content: string }
  | { type: "tool_call"; id: string; name: string; arguments?: unknown }
  | { type: "tool_call_response"; id: string; response: unknown }
  | { type: string; [key: string]: unknown };            // anything else: blocked
export type Message = { role: "system" | "user" | "assistant" | "tool"; parts: Part[] };
export type Entry = Message | { type: "placeholder"; name: string };

export type Column = {
  id: string;                      // uuidv7() from @/lib/uuid
  connectionId: string | undefined;
  model: string;
  temperature: string;             // kept as typed text; "" = unset
  maxTokens: string;               // "" = unset
};

export type Draft = {
  entries: Entry[];
  variables: Record<string, string>;
  placeholders: Record<string, string>;   // JSON text of a Message[] per placeholder
  tools: string;                           // JSON text of [{name, description?, parameters}], "" = none
  toolChoice: string;                      // "" | "auto" | "none" | "required" | a tool name
  responseFormat: string;                  // JSON text of {name, schema, strict?}, "" = none
  columns: Column[];                       // 1–4
  source?: { prompt: { name: string; version: number } } | { span: { trace_id: string; span_id: string } };
};

export type RunEvent =
  | { event: "start"; data: { trace_id: string } }
  | { event: "text"; data: { delta: string } }
  | { event: "tool_call"; data: { id: string; name: string; arguments: unknown } }
  | { event: "done"; data: { finish_reason: string | null; usage: Usage | null; cost_usd: number | null; latency_ms: number; ttft_ms: number | null; output: Message | null } }
  | { event: "error"; data: { class: "retryable" | "pause" | "permanent"; message: string } };
export type Usage = { input: number; output: number; cache_read: number; cache_write: number; reasoning: number };
```

Check every field name against `crates/server/src/routes/playground.rs`' event payloads and `api/playground.rs`' request; if one differs, follow the server and say so.

### Fixed texts

| Where | Text |
|---|---|
| sidebar, top bar | `Playground` |
| viewer reason | `Only members can run the playground` |
| blocked parts | `Message {n} has {kind} parts, which the playground cannot send. Remove them to run.` |
| save blocked | `Message {n} has tool parts; a prompt version holds text messages only.` |
| no connection | `Add an LLM connection in organization settings to run the playground.` (link to `/$org/settings/llm-connections`) |
| empty recent runs | `No playground runs yet.` |

## Precisions (from the cold review; they override the phase text where they differ)

1. **Run URL.** `const url = \`${window.location.origin}/api/v1/projects/${encodeURIComponent(project)}/playground/runs\``; `fetch(url, { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", Accept: "text/event-stream" }, body: JSON.stringify(body), signal })`. On 401 call `reportUnauthorized(new URL(url).pathname)`. On another non-2xx: `let problem; try { problem = await response.json() } catch {}`; throw `problem` when `isProblem(problem)`, else `new Error(\`HTTP ${response.status}\`)`.
2. **Ids.** `Column.id` comes from `uuidv7()` in `@/lib/uuid` (never `crypto.randomUUID`, missing over plain http).
3. **Draft persistence.** `saveDraft` runs on every change with a 500 ms debounce, and the pending save is flushed in the effect cleanup (unmount) and on `pagehide`. `loadDraft` validates the parsed value (entries an array, 1–4 columns, string fields; unknown extra fields ignored) and returns `undefined` on any mismatch (like `parseStore` in `recent-items.ts`). Keys use `STORAGE_PREFIX` from `@/product` (`${STORAGE_PREFIX}playground.<org>/<project>`). A stored `connectionId` that is not in the org's connections is reset to the first connection once they load.
4. **Parts.** Only `uri`, `blob` and unknown part types are blocked; `reasoning` parts are kept in the draft, sent (the server accepts and does not forward them), and dropped by `toChatTemplate`. `blockedMessages` also reports, per message, what the server refuses: a `tool` message with a text part, a `tool_call` outside an `assistant` message, a `tool_call_response` outside a `tool` message, a tool part without an `id`. The `Part` type uses `id?: string` for tool parts; conversion functions narrow with `part.type === "text" && typeof part.content === "string"`. A message with several text parts is shown joined with `"\n"` (and saved back as one text part when edited). `Message.name` and `finish_reason` are dropped. Message numbers in texts count every entry, placeholders included (the server's indexes).
5. **Tool messages.** A message with role `tool` is always edited as parts JSON; choosing role `tool` pre-fills `[{"type":"tool_call_response","id":"","response":""}]`.
6. **Continue the conversation.** Each column with a `done.output` has `Add answer to conversation`, which appends that assistant message (with its parts, reasoning dropped) to `draft.entries`.
7. **Literal braces (amends D-V-37; part of D-V-38).** The server request gains `render_variables: bool` (default `true`): when `false`, the request's messages are sent as written. V7.1 makes this server change: in `crates/server/src/api/playground.rs` add `#[serde(default = "yes")] pub render_variables: bool` (with `fn yes() -> bool { true }`, `skip_serializing_if` when true), and in `crates/server/src/routes/playground.rs` skip `render_messages`' variable rendering when false (placeholders are still expanded: call `render_messages` with an empty variable map and a flag, or add a `render: bool` parameter to `cetra_eval::playground::render_messages`, editing `crates/eval/src/playground.rs`); one route test each way; spec 09 § Playground gains the field. These Rust files join V7.1's Edit list. The UI: a `Treat {{ }} as variables` switch in the settings panel, on for an empty draft and a prompt source, off for a span or past-run source; `draftVariables` lists nothing when off; `toRunRequest` sends the flag. A 400 on `messages[i].parts[j]` (with its `position`) is shown under message i.
8. **`toRunRequest(draft, column, connections)`** resolves the model (the column's, else the connection's `default_model`, else the error `model`: "Choose a model"); error keys are unindexed (`model`, `connection`, `temperature`, `maxTokens`, `tools`, `responseFormat`, `placeholders.<name>`). Rules: send every `draftVariables` name with `variables[name] ?? ""`; send `placeholders[name]` only for placeholder names present in the entries (blank text → `[]`); tools: blank → omitted, else `JSON.parse` must give an array of objects whose `name` matches `^[A-Za-z0-9_-]{1,64}$`; `max_tokens` an integer 1–65536; `temperature` a number 0–2; `tool_choice` omitted when there are no tools (unless `none`); omit empty `params`, `tools`, `response_format`.
9. **Abort and stream end.** `runPlayground` resolves silently when aborted (catch `AbortError` from `fetch` and from `reader.read`; release the reader in `finally`). The column shows `Stopped` and keeps the partial text. A stream that ends with neither `done` nor `error` shows `The stream ended early.` A `Run` while running aborts the previous controller first. A 429 right after `Stop` (the server frees the slot after its commit) shows the problem message.
10. **`readSse`.** `TextDecoder` with `decode(chunk, { stream: true })` and a final `decode()`; split lines on `\r\n`, `\n` or `\r`; a line starting with `:` is ignored; `field: value` strips one leading space from the value; several `data:` lines join with `"\n"`; the event name defaults to `message`; a blank line dispatches; at the end of the stream a pending event without a blank line is dropped; `reader.cancel()` in `finally`. `runPlayground` skips an event whose `data` does not parse as JSON or whose name is unknown, and `ping`. Tests add a multi-byte character split across chunks, a CRLF stream and a two-line `data`.
11. **Screenshot.** `{ "name": "playground", "path": "/acme/support-bot/playground?prompt=support-reply&version=2", "waitFor": "Run all", "now": "2026-01-01T11:00:00Z", "actions": [{ "click": "button:has-text('Run all')" }], "waitAfter": "Trace" }` (`waitAfter` is a text, `click` a Playwright locator). The page imports the PNG; it does not exist until the milestone's check runs `just docs-screenshots playground`, so V7.5's commit leaves the docs build red until then (expected).
12. **Recent runs** use `api.GET("/api/v1/projects/{project}/traces", …)` directly through `useInfiniteQuery` (not `traceListOptions`), with `from` = now − 90 days and `to` = now (computed once when the panel opens), `limit: 50`, the filter of V7.3, and `staleTime: 0`; the list is also invalidated after each `done`/`error`. Rows show `start_time` (`RelativeTime`), `models[0]`, `cost_total`, and an error mark when `error_count > 0` (`TraceSummary` has no status).
13. **Reopening a run.** The trace detail query answers `{ spans: … }`: take the `llm` span's id (`spans.find((row) => row.span.span_kind === "llm")?.span.span_id`, check the real shape), then `spanDetailQuery`. `metadata["cetra.playground.source"]` is `prompt:<name>@<version>` or `span:<trace>:<span>`: parse it into `draft.source` (a malformed value is ignored) and show `From prompt <name> v<version>` / `From a span`. The column model is `request_model ?? model`. The connection is the org connection whose adapter matches the span's `provider` (`openai`, `anthropic`, `gcp.gemini` → `gemini`), else the first. A reopened run starts with `Treat {{ }} as variables` off. `response_format` and `tool_choice` are not stored on spans; the docs page says so.
14. **Async loading.** The page renders its workspace only after the connections query has loaded (a skeleton before). Sources (`prompt`/`version`, `trace`/`span`, `run`) load in an effect through `queryClient.fetchQuery`, guarded by a ref keyed on the source search, so StrictMode and the `replace: true` cleanup of the search keys neither reload nor loop. Removing the source keys from the URL after loading is a deviation from D-V-28 recorded in D-V-38.
15. **Types.** `Schema<"Message">`/`Schema<"Payload">` have `role: string`: convert to the plan's `Message`, clamping an unknown role to `user`. The run request is the plan's `RunRequest` (not the generated `PlaygroundRunRequest`, whose messages are untyped).
16. **Mutations** by path: `$api.useMutation("post", "/api/v1/projects/{project}/prompts")` and `…/prompts/{prompt}/versions`, as `new-prompt-dialog.tsx` does.
17. **Icons.** `MetricChip` takes an `icon`: `ArrowDownToLine`/`ArrowUpFromLine` (tokens), `Coins` (cost), `Timer` (latency), `Zap` (TTFT), `Flag` (finish reason); the nav icon is `SquareTerminal` (distinct from Experiments' flask). Check each exists in the installed lucide-react; if one does not, pick a close existing one and say so.
18. **Docs and decisions.** The changelog/roadmap text uses `#TBD` for the follow-up PR number. The ui-tour go-key table gets `g then l` (and `g then r` if M4's row is missing). D-V-38's text: raw fetch for runs; parts JSON editing for non-text messages; models free text with the connection's default; go-key `l`; `render_variables` (default true; off for span and run sources) amending D-V-37; the source search keys are removed after loading (the draft keeps the source), amending D-V-28.
19. **Tests' GETs.** Every page test mocks with `server.use` the GETs it needs whose fixture is not the default (the traces list with a filter, the trace and span detail for a run, a span with system instructions); the manifest serves `/api/auth/me`, the project, llm-connections, prompts, settings, prompt and versions.

---

## V7.1 — Run client, SSE parser, draft model and conversions

**Commit subject:** `web: add the playground run client, draft model and conversions`

### Read first

`web/src/api/client.ts` (credentials, base URL), `web/src/api/session.ts` (`reportUnauthorized`), `web/src/api/problem.ts`, `web/src/lib/sse.ts` (an existing SSE consumer, for style), `web/src/components/shell/recent-items.ts` (`localStorage` pattern), `web/src/lib/prompts.ts` (M4: `ChatEntry`, `isValidLabel`, `isValidPromptName`), `web/src/lib/datasets.ts` (`parseJsonField`).

### Edit

| File | Change |
|---|---|
| `web/src/lib/playground.ts`, `web/src/lib/playground.test.ts` | new |

### Functions (exactly these)

```ts
/** Reads `text/event-stream` bytes as events; `data` is the joined `data:` lines; ignores comments and `ping`. */
export async function* readSse(body: ReadableStream<Uint8Array>): AsyncGenerator<{ event: string; data: string }>;

/** POSTs one run. Before the stream: a non-2xx answer throws the parsed problem (a 401 also calls `reportUnauthorized`). Then calls `onEvent` for each parsed event until the stream ends. Aborting `signal` cancels. */
export async function runPlayground(project: string /* projectRef */, body: RunRequest, options: { signal: AbortSignal; onEvent: (event: RunEvent) => void }): Promise<void>;

export type RunRequest = { connection_id: string; model: string; messages: Entry[]; variables: Record<string, string>; placeholders: Record<string, Message[]>; params: { temperature?: number; max_tokens?: number }; tools: unknown[]; tool_choice?: string; response_format?: unknown; source?: Draft["source"] };

/** Builds a column's request; returns field errors (keys: `tools`, `responseFormat`, `placeholders.<name>`, `columns.<i>.temperature|maxTokens|model|connection`) instead when the draft is invalid. */
export function toRunRequest(draft: Draft, column: Column): { ok: true; request: RunRequest } | { ok: false; errors: Record<string, string> };

/** `{{ name }}` names in order of first use across the text parts (same rule as the SDKs; invalid tags are skipped). */
export function draftVariables(draft: Draft): string[];

/** Messages (1-based numbers) holding parts the server refuses (anything but text, tool_call, tool_call_response). */
export function blockedMessages(draft: Draft): { index: number; kinds: string[] }[];

/** A prompt version (M4's `Schema<"PromptVersion">`) as a draft: text template → one user message; chat → messages and placeholders; `config.model` and `config.params.temperature`/`max_tokens` into the first column, `config.connection_id` too; tools/tool_choice/response_format as pretty JSON; `source.prompt`. */
export function draftFromPromptVersion(version: Schema<"PromptVersion">, fallbackConnectionId: string | undefined): Draft;

/** A span detail as a draft: `system_instructions` (a string or JSON) → a system message first; `input` with format `messages` → messages (text format → one user message; json → one user message with the JSON text); `tool_definitions` (array or JSON string) → tools; `request_params.temperature`/`max_tokens` → the column; `model` → the column's model; `source.span`. */
export function draftFromSpan(span: Schema<"SpanDetail">, fallbackConnectionId: string | undefined): Draft;

/** The draft's messages as a chat template for "Save as new version": text-only messages become `{role, content}` (parts joined with "\n"), placeholders stay; a message with other parts → the 1-based number of the first one in `blocked`. Role `tool` is refused like tool parts. */
export function toChatTemplate(draft: Draft): { ok: true; template: ChatEntry[] } | { ok: false; blocked: number };

/** Per-project draft in localStorage (`cetra.playground.<org>/<project>`), try/catch with an in-memory fallback. */
export function loadDraft(projectKey: string): Draft | undefined;
export function saveDraft(projectKey: string, draft: Draft): void;

export function emptyDraft(connectionId: string | undefined): Draft;   // one system and one user message, one column
```

### Tests (`playground.test.ts`)

`readSse` over a `ReadableStream` built from chunks that split events and lines mid-way (start, ping, two text, done → three events in order, ping skipped); `runPlayground` with an MSW `server.use(http.post(…))` answering a stream (`new HttpResponse(stream, { headers: { "Content-Type": "text/event-stream" } })`): events delivered in order; a 400 problem answer throws with `fieldErrors` readable; a 401 calls `reportUnauthorized` (spy); abort → the request's `signal.aborted` is true (assert on the signal, not on reader cancellation). `toRunRequest`: numbers parsed, invalid JSON in tools → `tools` error, placeholder JSON parsed into messages, temperature `abc` → error. `draftVariables` (`{{a}} {{ b }} {{a}} {% x %}` → `["a","b"]`). `blockedMessages` (an image part). `draftFromPromptVersion` with the `support-reply` v2 fixture (`fixtures/api/responses/prompts/support-reply/versions/2.json`). `draftFromSpan` with a span having system instructions, messages and tool definitions as a JSON string. `toChatTemplate` blocked by a tool-call message and by role `tool`. `loadDraft`/`saveDraft` round-trip and survive a throwing `localStorage` (stub it to throw).

---

## V7.2 — The playground page: editor, columns, streaming

**Commit subject:** `web: add the playground page with streamed model columns`

### Read first

V7.1's file; `web/src/components/prompts/template-editor.tsx` (M4, for editor style); `web/src/api/queries-settings.ts` (`connectionsInit`), `web/src/api/queries.ts` (`meQuery`); `web/src/components/ui/resizable.tsx`, `select.tsx`, `textarea.tsx`; `web/src/components/trace/metric-chip.tsx`; `web/src/lib/format.ts` (`formatCost`, `formatTokens`, `formatDuration`); `web/src/components/shell/nav.ts`, `web/src/lib/shortcuts.ts` and their tests; an existing full-height page (`web/src/routes/$org/$project/sql/index.tsx`) for layout.

### Edit

| File | Change |
|---|---|
| `web/src/routes/$org/$project/playground/index.tsx`, `index.test.tsx` | new page |
| `web/src/components/playground/message-editor.tsx` | new |
| `web/src/components/playground/run-column.tsx` | new |
| `web/src/components/playground/settings-panel.tsx` | new (variables, placeholders, tools, tool choice, response format) |
| `web/src/components/shell/nav.ts`, `web/src/lib/shortcuts.ts`, `nav.test.ts`, `shortcuts.test.ts` | `Playground` (section Evaluate, after Prompts, icon `FlaskRound` or another lucide icon that exists — check the import), go-key `l` |

### Steps

1. **Route** `createFileRoute("/$org/$project/playground/")` with `validateSearch` → `{ prompt?: string; version?: number; trace?: string; span?: string; run?: string }` (strings ≤ 200 chars; `version` integer ≥ 1; `trace` 32 hex, `span` 16 hex, `run` 32 hex, else dropped).
2. **State**: `draft` in `useState`, initialized (once) from, in order: the `prompt`/`version` source (V7.4 wires the loading; in this phase only `loadDraft(projectRef)` or `emptyDraft(firstConnection)`), and saved with `saveDraft` (debounced 500 ms) on every change.
3. **Layout**: left a `ResizablePanel` with `MessageEditor` and `SettingsPanel`; right the columns (`RunColumn` × 1–4, side by side, each `min-w-72`, horizontal scroll if needed). Toolbar: `Run all` (disabled with the viewer reason below member; disabled while any column runs, with `Stop all` instead), `Add model` (≤ 4), and (V7.3) `Save as new version…`, `Recent runs`.
4. **MessageEditor**: an ordered list of entries; a text-only message: role `Select` (system/user/assistant/tool; `aria-label="Role of message {n}"`) and a `Textarea` (`aria-label="Text of message {n}"`, `font-mono text-body`); a message with other parts: role `Select` and a `Textarea` with the parts' pretty JSON (`aria-label="Parts of message {n}"`, parsed with `parseJsonField(…, { required: true })` into an array; error under it); a placeholder: `Placeholder` badge and a name `Input`. Buttons per entry: Move up, Move down, Remove; below: `Add message`, `Add placeholder`. Blocked messages (`blockedMessages`) show the blocked-parts text under them in `text-destructive` and disable every Run button with that text as `DisabledReason`.
5. **SettingsPanel**: `Variables` — one `Input` per `draftVariables(draft)` name (label = the name); `Placeholders` — one JSON `Textarea` per placeholder entry name; `Tools` (JSON `Textarea`), `Tool choice` (`Select`: none set, auto, none, required, each tool name), `Response format` (JSON `Textarea`). Field errors from `toRunRequest` show under their control.
6. **RunColumn** (`{ column, draft, onChange, onRemove, projectRef, connections, canRun }`): a connection `Select` (`aria-label="Connection"`, options from the org connections; none → the no-connection text with its link), a model `Input` (`aria-label="Model"`, placeholder = the connection's `default_model`, empty → use it), temperature and max tokens `Input`s, `Run` / `Stop` buttons. Running: `toRunRequest` (errors → shown, no request), an `AbortController`, `runPlayground(…)` with `onEvent`: `start` keeps the trace id; `text` appends to the output text (rendered in a `pre`, `whitespace-pre-wrap`, `aria-live="polite"`); `tool_call` adds a JSON block `Tool call: {name}`; `done` shows `MetricChip`s: tokens in/out, cost (`formatCost`), latency (`formatDuration(ms * 1e6)`), TTFT, finish reason, and a `Trace` link to `/$org/$project/traces/$traceId` (with `search: { eval: 1 }` if the traces page needs it to show reserved environments — check `lib/search/traces.ts`; otherwise the trace detail opens by id regardless); `error` shows `{message}` in `text-destructive` with the class as a badge; a thrown problem before the stream shows `problemMessage(error, "Could not start the run.")` and field errors mapped to the editor when their field is `messages[...]`/`variables`/`placeholders`/`tools`/`tool_choice`/`response_format`/`model`/`connection_id`/`params.*`. `Stop` aborts. Unmount aborts.
7. **Run all** starts every column's run at once (each its own request).
8. **Viewers** (`!canRunPlayground(role)`): editing works (local draft), every Run is disabled with the viewer reason.
9. **Nav**: the entry and go-key; `nav.test.ts` and `shortcuts.test.ts` gain the new target.

### Tests (`playground/index.test.tsx`; mock `GET /api/v1/orgs/:org/llm-connections` with `fixtures/api/static/llm-connections.json` (or the generated fixture the manifest serves) and every other GET the page makes)

1. `runs one column and streams the answer` — type a user message, run: the POST body has the message, the connection id, the model; the output shows the streamed text; the chips show cost and latency; a `Trace` link to the trace id from `start`.
2. `run all sends one request per column` — add a second column with another model; `Run all` → two POSTs with each model.
3. `variables are listed and sent` — a message with `{{product}}` shows a `product` input; its value is in `variables`.
4. `a 400 field error lands on the message` — the POST answers 400 with `messages[0].parts[0]`: the message under message 1.
5. `an error event is shown` — the stream sends `error {class: "pause", message: "…"}`: the message and the `pause` badge.
6. `stop cancels` — a stream that never ends; `Stop` → the request's signal is aborted.
7. `blocked parts disable running` — a draft (from `saveDraft` before render) with an image part: the blocked text and disabled Run.
8. `viewer cannot run` — role viewer: Run disabled with the viewer reason.
9. `the draft survives a reload` — edit, unmount, render again: the text is back.

---

## V7.3 — Save as new version, recent runs, reopening a run

**Commit subject:** `web: save playground drafts as prompt versions and list recent runs`

### Read first

V7.2's files; `web/src/api/queries-prompts.ts` (M4: `invalidatePrompt`, queries), `web/src/components/prompts/new-prompt-dialog.tsx` and `save-version-dialog.tsx` (M4, for the label and protected-label handling), `web/src/api/queries.ts` (traces list init, `spanDetailQuery`/trace detail), `web/src/components/data-table/use-infinite-list.ts`.

### Edit

| File | Change |
|---|---|
| `web/src/components/playground/save-dialog.tsx`, `save-dialog.test.tsx` | new |
| `web/src/components/playground/recent-runs.tsx`, `recent-runs.test.tsx` | new |
| `web/src/routes/$org/$project/playground/index.tsx` | toolbar buttons, `run` search handling |

### Steps

1. **Save dialog** (`Save as new version…`, disabled with the viewer reason when `!canManagePrompts(role)`): choose `Existing prompt` (a prompt name `Select` from the prompts list, chat prompts only; text prompts are listed disabled with `Text prompts take one text; save as a new prompt instead.`) or `New prompt` (name input, `isValidPromptName`); commit message; labels to set (as M4's save dialog: existing labels as checkboxes, protected ones disabled below admin with M4's protected reason; a new label input). `toChatTemplate(draft)` blocked → the save-blocked text and a disabled Save. Config sent: `{ model, connection_id, params: { temperature?, max_tokens? }, tools?, tool_choice?, response_format? }` from the first column and the settings (parsed JSON). Existing → `post_prompt_version`; new → `post_prompt` with `type: "chat"`. Success: `invalidatePrompt`, a `role="status"` line `Saved as {name} v{version}.` with a link to the prompt page `?version=`. Errors as M4's dialogs (field errors, 403 protected, 409, other → `problemMessage`).
2. **Recent runs** (a `Sheet` or side panel opened by `Recent runs`): the traces list query with `filter = environment = 'cetra-playground' AND metadata['cetra.playground.run_by'] = '<me.user.id>'` (single quotes; the user id from `meQuery`), newest first, 50 per page with `Load more`; each row: time (`RelativeTime`), model, cost, status, and `Open` (navigates to `?run=<trace_id>`). Empty: the fixed text.
3. **Reopen** (`?run=`): load the trace's spans (the trace detail query, then the one span's detail), build the draft with `draftFromSpan` (its `metadata['cetra.playground.source']` is shown as a line `From prompt support-reply v2` / `From a span` when present), replace the current draft after a `ConfirmDialog` when the current draft is not empty (`Replace the current draft?`), then remove `run` from the search.

### Tests

`save-dialog.test.tsx`: saving to an existing chat prompt sends `post_prompt_version` with the template and config; a tool-call message blocks Save with the text; a member cannot tick `production` (protected); a new prompt sends `post_prompt` with `type: "chat"`. `recent-runs.test.tsx`: the traces GET carries the exact filter with the user id; rows render; `Open` navigates to `?run=`. Page test: `?run=<id>` with mocked trace and span detail fills the editor with the span's messages.

---

## V7.4 — Entry points: prompt page, trace view, sources

**Commit subject:** `web: open the playground from prompt versions and LLM spans`

### Read first

`web/src/routes/$org/$project/prompts/$prompt.tsx` and `web/src/components/prompts/prompt-header.tsx` (M4); `web/src/components/trace/trace-view.tsx` (the `actions` passed to `TraceHeader` around lines 259–272: "Run evaluator", "Add to dataset"); V7.2's page.

### Edit

| File | Change |
|---|---|
| `web/src/components/prompts/prompt-header.tsx` | `Open in playground` (link to `/playground?prompt=<name>&version=<shown version>`) |
| `web/src/components/trace/trace-view.tsx` | `Open in playground` among the actions when the selected span's `span_kind` is `llm` (link to `/playground?trace=<id>&span=<id>`) |
| `web/src/routes/$org/$project/playground/index.tsx` | loading from `prompt`/`version` and from `trace`/`span` |
| tests: `web/src/components/trace/trace-view.test.tsx` (or the closest existing test of the trace actions), `$prompt.test.tsx`, `playground/index.test.tsx` | cases |

### Steps

1. Page loading, once per distinct source search, before falling back to the stored draft: `prompt`+`version` → `promptVersionQuery` → `draftFromPromptVersion` (missing version → latest via `promptQuery`); `trace`+`span` → `spanDetailQuery` → `draftFromSpan`; the first org connection is the fallback connection. When a stored draft is not empty, ask with `ConfirmDialog` (`Replace the current draft?`) before replacing. After loading, remove the source keys from the search (`replace: true`) but keep `draft.source`.
2. A span whose `truncated` list is not empty shows a banner `This span was cut at ingest; the playground shows what was stored.`
3. Buttons: plain `Link`s styled as buttons (`size="sm"`, `variant="outline"`), shown to every role (viewers can look; running is gated on the page).

### Tests

The prompt page has an `Open in playground` link with `prompt=support-reply&version=2`; the trace view shows the action for an `llm` span and not for a `chain` span; the playground page opened with `?prompt=support-reply&version=2` shows the v2 system text, the `history` placeholder and `gpt-4o` in the first column; opened with `?trace=…&span=…` (mocked span detail with system instructions and two messages) shows them.

---

## V7.5 — Dev mocks, docs, screenshots, spec, closeout

**Commit subject:** `Track V M7: playground mocks, docs, spec and closeout`

### Read first

`web/src/mocks/browser.ts`, `web/src/mocks/writes.ts` and `writes.test.ts`; `website/screenshots.json`, `website/scripts/screenshots.mjs`; `website/src/content/docs/docs/evaluation/prompts.mdx` (structure), `website/astro.config.mjs` (sidebar), `get-started/ui-tour.mdx`; `docs/spec/11-ui.md`; `docs/roadmap.md`; `docs/plans/v-prompts/00-design.md` (§ Open questions, decisions); `CHANGELOG.md`.

### Edit

| File | Change |
|---|---|
| `web/src/mocks/playground.ts` (new), `web/src/mocks/browser.ts` | a dev-server handler for `POST …/playground/runs` streaming a canned answer (start, three text deltas with 150 ms between them, done with usage 42/18, cost 0.0004, latency 900, ttft 300) |
| `web/src/mocks/writes.test.ts` | only if its allow-list must name the playground POST |
| `website/src/content/docs/docs/evaluation/playground.mdx` (new), `website/astro.config.mjs` | the page and its sidebar entry after Prompts |
| `website/screenshots.json`, `website/src/assets/screenshots/playground.png` | one shot |
| `website/src/content/docs/docs/evaluation/prompts.mdx` | a sentence linking to the playground |
| `website/src/content/docs/docs/get-started/ui-tour.mdx` | Playground in the Evaluate row, `g then l` |
| `docs/spec/11-ui.md` | navigation line, a `### Playground` section |
| `docs/mvp-decisions.md`, `docs/plans/v-prompts/00-design.md` | D-V-38; design status line `Status: implemented (M1–M7)` at the top |
| `CHANGELOG.md`, `docs/roadmap.md` | the line; the V row **Done** with the PR numbers |

### Steps

1. **Dev mock**: in `mocks/playground.ts`, `http.post("*/api/v1/projects/:project/playground/runs", …)` returning a `ReadableStream` that enqueues the SSE text of the canned events with `setTimeout` delays; register it in `mocks/browser.ts` before the generic handlers (never in `mocks/node.ts`: tests mock their own).
2. **Docs page** `evaluation/playground.mdx` (`title: Playground`, description ≤ 160 characters, `website/STYLE.md`): what it is; opening it (empty, from a prompt version, from an LLM span, from a recent run); messages, variables and placeholders; models side by side and what each column shows; tools and response formats (tools are not run; a tool-mode response format forces its own tool); limits (60 runs a minute and 4 at once per user, 16 per org; 300 s per call; connection limits shared with online evaluation); runs are traces in `cetra-playground`, hidden by default, not billed; saving as a prompt version; the screenshot. Code blocks (if any) carry `{/* untested: needs a running cetra server */}`.
3. **Screenshot**: `{ "name": "playground", "path": "/acme/support-bot/playground?prompt=support-reply&version=2", "waitFor": "support agent", "now": "2026-01-01T11:00:00Z", "actions": [{ "click": "Run all" }], "waitAfter": 1500 }` (match the file's real keys; check `screenshots.mjs` for `actions`/`waitAfter` support and adapt); run `just docs-screenshots playground` at the milestone's check (Chromium), not in this phase; commit the PNG then.
4. **Spec 11**: navigation `/:org/:project/playground   playground (track V, D-V-38)`; `### Playground` summarizing the page, sources, columns, saving, recent runs, entry points; cite D-V-20 to D-V-28, D-V-38.
5. **D-V-38** row (text of § Decisions' "Made precise" list) in both decision tables.
6. **Closeout**: changelog `### Added`: `- Playground (D-V-20 to D-V-28, D-V-38): run prompt versions and recorded LLM spans against up to four models side by side, streamed, with cost and latency; save the result as a prompt version; recent runs.`; roadmap V row status `**Done.** M1–M7 implemented (PR #181 for the design and M1–M3, M5, M6; the follow-up PR for M4 and M7).` (use the real PR numbers); the design doc's top status line.

### Milestone checks (after V7.5 is committed)

`pnpm --dir web gen:routes`, `just docs-screenshots playground` (and M4's two shots if still missing), then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`; fix and re-run only what failed; commit `Make track V M7 build`.
