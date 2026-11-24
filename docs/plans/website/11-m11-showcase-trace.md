# M11: A showcase trace for the hero

The hero screenshot (and the docs' `trace-view` shot) opens a procedurally generated trace from the web UI's mock dataset. Its spans have placeholder names (`tool 1`, `chain 2`, `llm 3`), random nesting and random times, so the tree and the timeline read as noise. This plan hand-writes one trace of the dataset: a support agent answering "How can I export my traces to a CSV file?", with real span names, models, tokens, costs and a believable waterfall with two parallel tool calls. The hero and the docs' trace view then show it.

**Prerequisites:** branch `claude/focused-screenshots` (PR #249) at or after `23dbe8d`. Playwright's Chromium for `playwright@1.65.1` is installed.

**Read first:**
- `docs/plans/website/README.md` § Conventions for every phase (they apply here too; this plan also edits `web/`, whose checks are named below).
- `web/src/mocks/generated/dataset.ts`, especially: `createDataset`, `traceSummary`, `buildDetail`, `treeSpan`, `spanFromSummary`, `turnOf`, `SUPPORT_TURNS` (topic 3 is "How can I export my traces to a CSV file?"), `userTraceMetadata`, `spanId`, `rfc3339`, `mix`.
- `web/src/mocks/generated/dataset.test.ts` and `handlers.test.ts`.
- `website/screenshots.json`, `website/src/marketing/home/Hero.astro`.

## What it is, and what it is not

When it's done:
- Trace index 3 of every generated dataset (`createDataset`) is the hand-written showcase trace, `support-agent`, 2 minutes old, with 13 spans. Every other trace is generated as before: each trace's random numbers come from `mix(seed, index)`, so the other indices are unchanged.
- The hero screenshot opens it (Timeline tab on), and so does the docs' `trace-view` shot: both click its row by name in the trace list.
- The docs and marketing screenshots that show the generated dataset are recaptured.

Out of scope: any other trace, the small fixtures under `fixtures/api/`, and the UI itself.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-12 | Screenshots come from the web UI on its MSW mocks |
| D-W-23 | Marketing shots are crops of one panel; the hero keeps its full-width picture under the text, with the timeline on |

The user's answer (2026-11-24): make the hero trace a "wow" moment with better span names and timeline; the scenario is the **support agent** (guardrail, intent, retrieval with embed/search/rerank, plan, two parallel tools, a long answer, a PII scrub, a judge), which fits the existing `support-bot` data.

## Versions

No new dependency.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| T1 The showcase trace | `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts` | — |
| T2 Recapture | `website/screenshots.json`, `website/src/marketing/home/Hero.astro`, the PNGs `just docs-screenshots` rewrites under `website/src/assets/screenshots/` | T1 |

### T1 The showcase trace

**Goal:** `createDataset` returns, at index 3, the hand-written `support-agent` trace below, consistent between its summary, its tree and its span details.

**Edit:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts`. No other file.

**Steps:**
1. Add these constants after `BUCKET_MS` in `dataset.ts`, exactly (Biome may reformat):
   ```ts
   /** The hand-written trace the homepage hero and the docs' trace view show (M11). Its topic is `SUPPORT_TURNS[3]`. */
   const SHOWCASE_INDEX = 3;
   const SHOWCASE_AGE_MS = 120_000;
   const SHOWCASE_DURATION_MS = 6_200;
   const SHOWCASE_USER = "user-012";

   type ShowcaseSpan = {
     name: string;
     kind: Schema<"SpanKind">;
     /** Index of the parent in SHOWCASE_SPANS; absent for the root. */
     parent?: number;
     /** Offsets from the trace's start, in ms. */
     from: number;
     to: number;
     model?: string;
     provider?: string;
     inputTokens?: number;
     outputTokens?: number;
     cost?: number;
     ttftMs?: number;
     prompt?: { name: string; version: number };
   };

   const SHOWCASE_SPANS: readonly [ShowcaseSpan, ...ShowcaseSpan[]] = [
     { name: "support-agent", kind: "agent", from: 0, to: 6_200 },
     { name: "prompt-injection check", kind: "guardrail", parent: 0, from: 20, to: 140 },
     { name: "classify intent", kind: "llm", parent: 0, from: 150, to: 560, model: "claude-haiku-4-5", provider: "anthropic", inputTokens: 412, outputTokens: 9, cost: 0.000457, ttftMs: 310 },
     { name: "retrieve docs", kind: "retriever", parent: 0, from: 580, to: 1_480 },
     { name: "embed query", kind: "embedding", parent: 3, from: 590, to: 760, model: "text-embedding-3-small", provider: "openai" },
     { name: "vector search", kind: "retriever", parent: 3, from: 770, to: 1_150 },
     { name: "rerank", kind: "reranker", parent: 3, from: 1_160, to: 1_470, model: "rerank-v3.5", provider: "cohere" },
     { name: "plan", kind: "llm", parent: 0, from: 1_500, to: 2_600, model: "claude-sonnet-4-5", provider: "anthropic", inputTokens: 1_840, outputTokens: 156, cost: 0.00786, ttftMs: 420 },
     { name: "get_account", kind: "tool", parent: 0, from: 2_620, to: 2_910 },
     { name: "check_export_limits", kind: "tool", parent: 0, from: 2_620, to: 3_230 },
     { name: "draft answer", kind: "llm", parent: 0, from: 3_250, to: 5_560, model: "claude-sonnet-4-5", provider: "anthropic", inputTokens: 2_310, outputTokens: 248, cost: 0.01065, ttftMs: 380, prompt: { name: "support-reply", version: 2 } },
     { name: "pii scrub", kind: "guardrail", parent: 0, from: 5_570, to: 5_650 },
     { name: "judge helpfulness", kind: "evaluator", parent: 0, from: 5_660, to: 6_190, model: "gpt-4.1-mini", provider: "openai", inputTokens: 690, outputTokens: 42, cost: 0.000343 },
   ];
   ```
   Every child's `[from, to]` lies inside its parent's, and every parent comes before its children (T1's tests check both).
2. **Summary.** In `traceSummary(seed, index, now)`, at the top, `if (index === SHOWCASE_INDEX) return showcaseSummary(index, now);`. Write `showcaseSummary(index: number, now: number): Schema<"TraceSummary">`:
   - `trace_id`: `` `a${index.toString(16).padStart(31, "0")}` `` (as the others); `root_span_id: spanId(index, 0)`; `has_root: true`; `name: "support-agent"`.
   - start `now - SHOWCASE_AGE_MS`, end start + `SHOWCASE_DURATION_MS`, `duration_ns` from it; `status: "ok"`; `environment: "production"`; `release: "v1.5.0"`; `tags: ["email"]`.
   - `metadata: userTraceMetadata(SHOWCASE_USER)`; `user_id: SHOWCASE_USER`; **no** `session_id` (so its turn is `turnOf(3, 1)`, the CSV topic's own question).
   - `span_count: SHOWCASE_SPANS.length` (13); `error_count: 0`; `llm_count`: the number of spans with `kind === "llm"` (3).
   - `input_tokens`, `output_tokens`: the sums over SHOWCASE_SPANS (5,252 and 455); `total_tokens` their sum; `cost_total`: the sum of `cost`, rounded to 6 decimals (`Math.round(sum * 1e6) / 1e6`).
   - `models: ["claude-sonnet-4-5", "claude-haiku-4-5", "gpt-4.1-mini"]`: the generation models only, on purpose; the embedding and rerank models appear on their spans, not in the trace's list.
   - `scores`: `[{ name: "correctness", score_id: scoreUuid(index, 0), source: "llm_judge", value: { data_type: "numeric", value: 0.9 } }, { name: "relevance", score_id: scoreUuid(index, 1), source: "human", value: { data_type: "boolean", value: true } }]`.
   `spreadSessionTurns` and `withPreviews` then treat it like any session-less trace (its previews come from topic 3).
3. **Tree.** In `buildDetail`, at the top, `if (index === SHOWCASE_INDEX) return showcaseDetail(index, trace);`. Write `showcaseDetail(index: number, trace: Schema<"TraceSummary">): Schema<"TraceDetail">` returning `{ trace, spans, truncated: false }`, where `spans` maps SHOWCASE_SPANS in order to `Schema<"TreeSpan">`:
   - `depth`: 0 for the root, else the parent's depth + 1; `detached: false`.
   - `span`: built like `treeSpan` builds its summary (same `trace_id`, `metadata`, `tags`, `environment`, `service_name: "support-bot"`, `user_id`, `release`, `status: "ok"`), with `span_id: spanId(index, i)`, `parent_span_id: spanId(index, parent)` when there is a parent, `span_kind: kind`, `name`, `start_time`/`end_time` from the trace's start + `from`/`to` (`rfc3339`), `duration_ns: (to - from) * 1_000_000`; and, when set on the ShowcaseSpan, `model`, `provider`, `input_tokens`, `output_tokens`, `total_tokens` (their sum), `cost_total: cost`, `ttft_ns: ttftMs * 1_000_000`, `prompt_name`/`prompt_version`.
   - The root (`i === 0`) also carries `input_preview`, `output_preview`, `input_tokens`, `output_tokens`, `total_tokens` and `cost_total` from the trace summary, as `treeSpan` does for every root.
4. **Payloads.** `spanFromSummary` already gives the right payloads for most kinds from the trace's turn (root and `agent`: question and short answer; `llm`: messages and the Markdown answer with a code block; `retriever`, `embedding`, `reranker`: the topic's docs). At its end, before `return detail;`, add `if (context.traceIndex === SHOWCASE_INDEX) applyShowcasePayload(detail, index, context.turn.exchange.question);` and write `applyShowcasePayload(detail: Schema<"SpanDetail">, index: number, question: string): void`. It first sets `detail.truncated = []` for every showcase span (the generic rule flags every 17th span's input, the root included, and the hero must not show a truncation badge). Then it overrides only these span indices (others keep the generic payload):
   - **1 `prompt-injection check`**: `input` text = the question; `output` json `{ passed: true, reasons: [] }`.
   - **11 `pii scrub`**: `input` json `{ text: "Export from the Traces page, or call the export endpoint for large ranges." }` (json, not text: the existing topic test reads a text input as the span's question); `output` json `{ passed: true, redacted: 0 }`.
   - **2 `classify intent`**: `input` messages: system `"Classify the user's request into one intent: billing, account, export_data, api_errors, other. Reply with JSON."`, user = the question. `output` messages: assistant `` '{"intent": "export_data", "confidence": 0.97}' ``.
   - **7 `plan`**: `input` messages: system `"You are a support agent. Plan the tool calls you need before answering."`, user = the question. `output` messages: assistant, one text part with these lines joined by `"\n"`: `"1. get_account: check the plan's export limits."`, `"2. check_export_limits: rows and formats allowed."`, `"3. Answer with the Traces page steps and the API for large ranges."`.
   - **8 `get_account`**: `input` json `{ name: "get_account", arguments: { user_id: "user-012" } }`; `output` json `{ plan: "pro", seats: 8, region: "eu1" }`.
   - **9 `check_export_limits`**: `input` json `{ name: "check_export_limits", arguments: { plan: "pro", format: "csv" } }`; `output` json `{ max_rows: 100000, formats: ["csv", "ndjson"], retention_days: 7 }`.
   - **12 `judge helpfulness`**: `input` text = the question; `output` json `{ score: 0.9, reasoning: "The answer gives the steps and the API route for large exports." }`.
   For the messages payloads, use the same shape `spanFromSummary` builds for `llm` (`{ format: "messages", value: [{ role, parts: [{ type: "text", content }] }] }`). Span 10 (`draft answer`) keeps the generic `llm` payload: the CSV topic's Markdown answer with its code block.
5. **Tests** (add to `dataset.test.ts`, with `createDataset({ seed: 1, now: NOW, traces: 10 })` unless said):
   - `"makes trace 3 the hand-written showcase trace"`: its `name` is `support-agent`, no `session_id`, `span_count` 13 equals the detail's span count, `llm_count` 3 equals the llm spans, `input_tokens`/`output_tokens`/`cost_total` equal the sums over the detail's non-root spans (cost to 6 decimals), `input_preview` is `"How can I export my traces to a CSV file?"`, and it starts `SHOWCASE_AGE_MS` (120,000 ms) before `NOW`.
   - `"nests every showcase span inside its parent, in pre-order"`: for every span with a parent, the parent appears earlier in the list, `depth` is the parent's + 1, and its start/end lie within the parent's.
   - `"runs the two showcase tools in parallel"`: `get_account` and `check_export_limits` have the same `start_time`.
   - `"gives the showcase spans their payloads"`: `classify intent`'s output text contains `export_data`; `check_export_limits`'s output has `max_rows: 100000`; `judge helpfulness`'s output has `score: 0.9`; `draft answer`'s output contains a fenced code block (as the generic `llm` test checks).
   - `"gives no showcase span a truncation flag"`: every showcase span's detail has `truncated` equal to `[]`.
   The existing tests must still pass unchanged, notably "gives every span of a trace the trace's topic" (it walks traces 1–3; every question found in the showcase's payloads is the CSV question) and "picks a different topic for a different trace" (traces 1 and 2).

**Done when:** from the worktree root, `pnpm --dir web exec vitest run src/mocks src/routes/\$org/\$project/traces src/test/quality-bar.test.tsx --maxWorkers=8` passes, and `pnpm --dir web run typecheck` and `./node_modules/.bin/biome check web/src/mocks/generated/dataset.ts web/src/mocks/generated/dataset.test.ts` report no error (run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first; quote the vitest path in single quotes outside bash). (`just check-web` runs once at the end of T2.)

**Commit:** `Add a hand-written showcase trace to the mock dataset`

### T2 Recapture

**Goal:** the hero and the docs' screenshots show the showcase trace.

**Edit:** `website/screenshots.json` (the `marketing-trace-view` and `trace-view` entries), `website/src/marketing/home/Hero.astro` (the Frame's `alt`), and the PNGs `just docs-screenshots` rewrites. No other file.

**Steps:**
1. In `marketing-trace-view` and in `trace-view`, replace the first action's `"text=How can I export my traces to a CSV file?"` with `"div[role=\"grid\"][aria-label=\"Traces\"] >> text=support-agent"`. The grid scope matters: the Name facet panel comes before the list in the page, and a plain `text=support-agent` would click its checkbox if the facet ever listed it. Keep `marketing-trace-view`'s Timeline action, viewport and clip, and `trace-view`'s other keys.
2. In `Hero.astro`, set the Frame's `alt` to `"The cetra trace view: a support agent's run, with its span tree, timeline, models, tokens and costs, and the root span's input and output."`.
3. Run `just docs-screenshots` (every shot: the dataset change moves the trace list, the sessions, the users and the dashboards a little). If port 5199 is busy, set `CETRA_SCREENSHOTS_PORT=5299`.
4. Look at every PNG that changed (`git status`), with the Read tool. Check that the hero's root span shows no truncation badge. Expected: `marketing-trace-view.png` and `trace-view.png` show `support-agent` with the 13 named spans; the trace-list shots show a `support-agent` row near the top; the others change only in counts or rows. Report any shot that changed in another way (a different page, an error, an empty state).

**Done when:** `just check-web` and `just check-docs` pass.

**Commit:** `Show the showcase trace in the hero and the docs`
