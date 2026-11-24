# M12: Hero scenarios

The hero shows one hand-written trace, a support agent (M11). One support bot makes cetra look like a support-bot tool. This plan adds three more hand-written traces to the web UI's mock dataset: a coding agent whose first test run fails, a research agent that searches in parallel, and an invoice extraction pipeline whose small model fails a schema check. It captures one hero screenshot of each, and puts labelled tabs above the hero picture to switch between the four (D-W-24).

**Prerequisites:** branch `claude/focused-screenshots` (PR #249) at or after the commit that adds this plan. Main's trace tree and timeline redesign (#250) is merged into it. Playwright's Chromium for `playwright@1.65.1` is installed.

**Read first:**
- `docs/plans/website/README.md` § Conventions for every phase. They apply here; this plan also edits `web/`, whose checks are named in E1.
- `docs/plans/website/11-m11-showcase-trace.md`: how the support-agent trace was added. This plan generalizes it.
- `web/src/mocks/generated/dataset.ts`, especially:
  - the `SHOWCASE_*` constants, `ShowcaseSpan` and `SHOWCASE_SPANS`;
  - `createDataset`, `traceSummary`, `showcaseSummary`, `buildDetail`, `showcaseDetail`;
  - `spanFromSummary`, `applyShowcasePayload`, `withPreviews`, `firstLine`;
  - `userTraceMetadata`, `spanId`, `spanIndex`, `textPayload`, `scoreUuid`, `rfc3339`.
- `web/src/mocks/generated/dataset.test.ts`: the showcase tests at the end, and "gives a trace without a session a topic of its own".
- `website/screenshots.json` (the `marketing-trace-view` entry), `website/src/marketing/home/Hero.astro`, `website/src/marketing/components/Tabs.astro`, `website/src/marketing/components/Frame.astro`, and the `.tablist` rules in `website/src/marketing/styles/base.css`.

## What it is, and what it is not

When it's done:
- Traces 4, 5 and 6 of every generated dataset are the hand-written `coding-agent`, `research-agent` and `extract-invoice` traces, 150, 180 and 210 seconds old. Trace 3 stays the support agent.
  - Every other trace is unchanged: each trace's random numbers come from `mix(seed, index)`.
  - The four hand-written traces are within the trace list's first loaded page (100 rows).
- Each new trace has hand-written payloads on every span, so no support-bot text shows inside it.
- `website/screenshots.json` has three new hero shots, one per new trace, captured like `marketing-trace-view`.
- The hero shows tabs above its picture: Support agent, Coding agent, Research agent, Document extraction.
  - Each tab shows its screenshot in the window frame, with a one-line caption under it.
  - Only the first picture loads with the page.

Out of scope:
- One scenario per product chapter (design §10 M12 row, possible later).
- The docs' own screenshots, other than recapturing them because the dataset changed.
- The UI itself.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-12 | Screenshots come from the web UI on its MSW mocks |
| D-W-23 | Marketing shots are crops of one panel; the hero keeps its full-width picture under the text, with the timeline on |
| D-W-24 | The hero shows four example traces behind labelled tabs, each with a caption; only the first picture loads with the page |

The user's answers (2026-11-24):
- Show several real uses in the hero, as labelled tabs: option A of the options shown. Carousel dots and a scroll strip were rejected.
- The scenarios are the four named above, with the stories described in this plan.
- Implementation by GLM.

A planner's reading, not asked:
- The coding agent's first `run_tests` span has status `error`, as a real failing tool call would.
- So the trace's status is `error` and its error count is 1. Spec 01 says a trace is `error` when any span errored.
- The trace header then shows a red status, which is honest: the agent recovered, but one call failed.

No `CHANGELOG.md` line: the product doesn't change. The mock dataset only feeds the UI's tests, the screenshots and the demo's sample data, and the site isn't in the changelog.

## Versions

No new dependency.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| E1 Scenario traces | `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts` | — |
| E2 Scenario screenshots | `website/screenshots.json`, the PNGs `just docs-screenshots` writes under `website/src/assets/screenshots/` | E1 |
| E3 Hero tabs | `website/src/marketing/home/Hero.astro`, `docs/plans/website/README.md` | E2 |

They run one after the other.

### E1 Scenario traces

**Goal:** `createDataset` returns the three new hand-written traces at indices 4, 5 and 6. Each one's summary, tree and span details agree, and the support agent at index 3 is unchanged.

**Edit:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/dataset.test.ts`. No other file.

**Steps:**

1. **Types.** In `dataset.ts`, replace the four `SHOWCASE_*` constants (`SHOWCASE_INDEX`, `SHOWCASE_AGE_MS`, `SHOWCASE_DURATION_MS`, `SHOWCASE_USER`) and the `ShowcaseSpan` type with the code below. Keep the support agent's span list as it is, renamed `SUPPORT_SPANS`, and keep its type annotation `readonly [ShowcaseSpan, ...ShowcaseSpan[]]`.
   ```ts
   type ShowcaseSpan = {
     name: string;
     kind: Schema<"SpanKind">;
     /** Index of the parent in the showcase's spans; absent for the root. */
     parent?: number;
     /** Offsets from the trace's start, in ms. */
     from: number;
     to: number;
     /** Absent: "ok". */
     status?: "error";
     /** The span's `status_message`; set on error spans. */
     statusMessage?: string;
     model?: string;
     provider?: string;
     inputTokens?: number;
     outputTokens?: number;
     cost?: number;
     ttftMs?: number;
     prompt?: { name: string; version: number };
   };

   /** A span's hand-written payloads; `error` adds the span's exception event. */
   type ShowcasePayload = {
     input: Schema<"Payload">;
     output: Schema<"Payload">;
     error?: { type: string; message: string; stacktrace: string };
   };

   /**
    * A hand-written trace of the mock dataset (M11, M12). The homepage hero shows them, and the docs' trace view
    * shows the support agent. The trace's duration is its root span's `to`.
    */
   type Showcase = {
     index: number;
     ageMs: number;
     service: string;
     user?: string;
     tags: string[];
     /** The generation models only; embedding and rerank models appear on their spans. */
     models: string[];
     scores: { name: string; source: Schema<"ScoreSource">; value: Schema<"ScoreValue"> }[];
     spans: readonly [ShowcaseSpan, ...ShowcaseSpan[]];
     /**
      * The trace's input and output previews, and every span's payloads by position in `spans`. Absent for the
      * support agent, whose previews and generic payloads come from its support turn (`SUPPORT_TURNS[3]`).
      */
     story?: { inputPreview: string; outputPreview: string; payloads: readonly ShowcasePayload[] };
   };
   ```
   `story.payloads[i]` belongs to `spans[i]`. The two arrays have the same length; a test checks it. Keep `Showcase`, `ShowcaseSpan` and `ShowcasePayload` unexported; a test that needs the type writes `(typeof SHOWCASES)[number]`.

2. **Payload helpers.** Add these after `textPayload`, exactly (Biome may reformat):
   ```ts
   function jsonPayload(value: unknown): Schema<"Payload"> {
     return { format: "json", value };
   }

   /** An llm span's payloads: a system and a user message in, one assistant message out. */
   function chatPayload(system: string, user: string, answer: string): ShowcasePayload {
     return {
       input: {
         format: "messages",
         value: [
           { role: "system", parts: [{ type: "text", content: system }] },
           { role: "user", parts: [{ type: "text", content: user }] },
         ],
       },
       output: {
         format: "messages",
         value: [{ role: "assistant", parts: [{ type: "text", content: answer }] }],
       },
     };
   }

   /** A tool span's payloads: the call as JSON in, `output` out. */
   function toolPayload(
     name: string,
     args: Record<string, unknown>,
     output: Schema<"Payload">,
   ): ShowcasePayload {
     return { input: jsonPayload({ name, arguments: args }), output };
   }
   ```

3. **The showcases.** Add the code below after `SUPPORT_SPANS`, exactly (Biome may reformat). Text in these strings is product copy: keep every character, including the backticks, the `·` and the `×`.
   - It uses `textPayload`, `jsonPayload`, `chatPayload`, `toolPayload` and `firstLine`. These are function declarations, which are hoisted, so the order in the file doesn't matter. Their bodies don't read any `const` declared after them.
   - `SHOWCASES` is exported for the tests only.
   ```ts
   const SUPPORT_SHOWCASE: Showcase = {
     index: 3,
     ageMs: 120_000,
     service: "support-bot",
     user: "user-012",
     tags: ["email"],
     models: ["claude-sonnet-4-5", "claude-haiku-4-5", "gpt-4.1-mini"],
     scores: [
       { name: "correctness", source: "llm_judge", value: { data_type: "numeric", value: 0.9 } },
       { name: "relevance", source: "human", value: { data_type: "boolean", value: true } },
     ],
     spans: SUPPORT_SPANS,
   };

   const CODING_QUESTION =
     "Fix the CSV export: fields that contain a comma break the columns (issue #412).";
   const CODING_ANSWER = [
     "Fixed `escape_field` so it quotes fields that contain a comma, a quote or a line break.",
     "",
     "- Patched `src/export/csv.py`",
     "- Updated the expected row in `tests/test_csv.py`",
     "- 48 tests pass",
   ].join("\n");
   const PATCH_SYSTEM = "Write a unified diff that fixes the issue.";

   const CODING_SHOWCASE: Showcase = {
     index: 4,
     ageMs: 150_000,
     service: "code-agent",
     user: "user-007",
     tags: ["retry"],
     models: ["claude-sonnet-4-5", "claude-haiku-4-5"],
     scores: [{ name: "tests_pass", source: "check", value: { data_type: "boolean", value: true } }],
     spans: [
       { name: "coding-agent", kind: "agent", from: 0, to: 18_400 },
       { name: "plan", kind: "llm", parent: 0, from: 120, to: 2_300, model: "claude-sonnet-4-5", provider: "anthropic", inputTokens: 3_120, outputTokens: 210, cost: 0.01251, ttftMs: 520 },
       { name: "gather context", kind: "chain", parent: 0, from: 2_330, to: 2_640 },
       { name: "read_file", kind: "tool", parent: 2, from: 2_340, to: 2_410 },
       { name: "search_code", kind: "tool", parent: 2, from: 2_340, to: 2_630 },
       { name: "attempt 1", kind: "chain", parent: 0, from: 2_660, to: 10_240 },
       { name: "write patch", kind: "llm", parent: 5, from: 2_670, to: 6_900, model: "claude-sonnet-4-5", provider: "anthropic", inputTokens: 5_400, outputTokens: 620, cost: 0.0255, ttftMs: 610 },
       { name: "apply_patch", kind: "tool", parent: 5, from: 6_920, to: 7_010 },
       { name: "run_tests", kind: "tool", parent: 5, from: 7_030, to: 10_230, status: "error", statusMessage: "1 failed, 47 passed" },
       { name: "attempt 2", kind: "chain", parent: 0, from: 10_260, to: 17_060 },
       { name: "fix failing test", kind: "llm", parent: 9, from: 10_270, to: 13_700, model: "claude-sonnet-4-5", provider: "anthropic", inputTokens: 7_100, outputTokens: 380, cost: 0.027, ttftMs: 640 },
       { name: "apply_patch", kind: "tool", parent: 9, from: 13_720, to: 13_790 },
       { name: "run_tests", kind: "tool", parent: 9, from: 13_810, to: 17_050 },
       { name: "summarize", kind: "llm", parent: 0, from: 17_080, to: 18_350, model: "claude-haiku-4-5", provider: "anthropic", inputTokens: 1_900, outputTokens: 120, cost: 0.0025, ttftMs: 280 },
     ],
     story: {
       inputPreview: CODING_QUESTION,
       outputPreview: firstLine(CODING_ANSWER),
       payloads: [
         { input: textPayload(CODING_QUESTION), output: textPayload(CODING_ANSWER) },
         chatPayload(
           "You are a coding agent. Plan the steps, then use the tools to read, edit and test the code.",
           CODING_QUESTION,
           [
             "1. Find where CSV fields are escaped.",
             "2. Quote fields that contain a comma, a quote or a line break.",
             "3. Run the CSV tests.",
           ].join("\n"),
         ),
         {
           input: jsonPayload({ goal: "Find the CSV escaping code." }),
           output: jsonPayload({ files: ["src/export/csv.py", "tests/test_csv.py"] }),
         },
         toolPayload(
           "read_file",
           { path: "src/export/csv.py" },
           textPayload(
             ["def escape_field(value: str) -> str:", "    return value.replace('\"', '\"\"')"].join("\n"),
           ),
         ),
         toolPayload(
           "search_code",
           { query: "escape_field" },
           jsonPayload({
             matches: [
               { path: "src/export/csv.py", line: 12 },
               { path: "tests/test_csv.py", line: 30 },
             ],
           }),
         ),
         { input: jsonPayload({ attempt: 1 }), output: jsonPayload({ tests: "failed" }) },
         chatPayload(
           PATCH_SYSTEM,
           "Quote the fields that contain a comma, a quote or a line break in escape_field.",
           [
             "```diff",
             "-    return value.replace('\"', '\"\"')",
             "+    escaped = value.replace('\"', '\"\"')",
             "+    return f'\"{escaped}\"' if needs_quotes(value) else escaped",
             "```",
           ].join("\n"),
         ),
         toolPayload("apply_patch", { path: "src/export/csv.py" }, jsonPayload({ applied: true, hunks: 1 })),
         {
           ...toolPayload(
             "run_tests",
             { command: "pytest tests/test_csv.py" },
             textPayload(
               [
                 "FAILED tests/test_csv.py::test_comma_field - assert '\"a,b\"' == 'a,b'",
                 "1 failed, 47 passed in 3.1s",
               ].join("\n"),
             ),
           ),
           error: {
             type: "TestFailure",
             message: "1 failed, 47 passed",
             stacktrace: "tests/test_csv.py:31: AssertionError",
           },
         },
         { input: jsonPayload({ attempt: 2 }), output: jsonPayload({ tests: "passed" }) },
         chatPayload(
           PATCH_SYSTEM,
           "test_comma_field expects the old, unquoted output. Update the test.",
           [
             "```diff",
             '-    assert escape_field("a,b") == "a,b"',
             "+    assert escape_field(\"a,b\") == '\"a,b\"'",
             "```",
           ].join("\n"),
         ),
         toolPayload("apply_patch", { path: "tests/test_csv.py" }, jsonPayload({ applied: true, hunks: 1 })),
         toolPayload(
           "run_tests",
           { command: "pytest tests/test_csv.py" },
           textPayload("48 passed in 3.2s"),
         ),
         chatPayload(
           "Summarize the change for the pull request.",
           "Summarize the patch and the test runs.",
           CODING_ANSWER,
         ),
       ],
     },
   };

   const RESEARCH_QUESTION =
     "What are the current best practices to prevent CSV injection in exported files?";
   const RESEARCH_ANSWER = [
     "Neutralize the cells a spreadsheet would read as a formula, and quote every field.",
     "",
     "- Prefix cells that start with `=`, `+`, `-`, `@`, a tab or a carriage return with a single quote [1]",
     "- Quote every field, and double the quotes inside it [2]",
     "- Escape on export, and keep the raw value in the API [1]",
     "",
     "[1] OWASP, CSV Injection · [2] RFC 4180",
   ].join("\n");
   const RESEARCH_QUERIES = [
     "csv injection prevention",
     "owasp csv injection",
     "rfc 4180 quoting",
     "spreadsheet formula injection export",
     "csv injection escape single quote",
   ] as const;
   const OWASP_CSV = {
     title: "CSV Injection",
     url: "https://owasp.org/www-community/attacks/CSV_Injection",
   };
   const RFC_4180 = {
     title: "RFC 4180: Common Format and MIME Type for CSV Files",
     url: "https://www.rfc-editor.org/rfc/rfc4180",
   };

   const RESEARCH_SHOWCASE: Showcase = {
     index: 5,
     ageMs: 180_000,
     service: "research",
     user: "user-021",
     tags: [],
     models: ["claude-sonnet-4-5", "gpt-4.1"],
     scores: [{ name: "groundedness", source: "llm_judge", value: { data_type: "numeric", value: 0.95 } }],
     spans: [
       { name: "research-agent", kind: "agent", from: 0, to: 24_600 },
       { name: "plan queries", kind: "llm", parent: 0, from: 100, to: 1_900, model: "gpt-4.1", provider: "openai", inputTokens: 820, outputTokens: 140, cost: 0.00276, ttftMs: 450 },
       { name: "search", kind: "chain", parent: 0, from: 1_950, to: 7_310 },
       { name: "web_search", kind: "tool", parent: 2, from: 1_960, to: 5_200 },
       { name: "web_search", kind: "tool", parent: 2, from: 1_960, to: 6_100 },
       { name: "web_search", kind: "tool", parent: 2, from: 1_960, to: 4_700 },
       { name: "web_search", kind: "tool", parent: 2, from: 1_960, to: 7_300 },
       { name: "web_search", kind: "tool", parent: 2, from: 1_960, to: 5_600 },
       { name: "read sources", kind: "chain", parent: 0, from: 7_350, to: 11_200 },
       { name: "fetch_page", kind: "tool", parent: 8, from: 7_360, to: 9_800 },
       { name: "fetch_page", kind: "tool", parent: 8, from: 7_360, to: 11_150 },
       { name: "write report", kind: "llm", parent: 0, from: 11_250, to: 23_900, model: "claude-sonnet-4-5", provider: "anthropic", inputTokens: 14_200, outputTokens: 1_650, cost: 0.06735, ttftMs: 900 },
       { name: "check citations", kind: "guardrail", parent: 0, from: 23_950, to: 24_500 },
     ],
     story: {
       inputPreview: RESEARCH_QUESTION,
       outputPreview: firstLine(RESEARCH_ANSWER),
       payloads: [
         { input: textPayload(RESEARCH_QUESTION), output: textPayload(RESEARCH_ANSWER) },
         chatPayload(
           "You are a research agent. Write the web searches you need, one per line.",
           RESEARCH_QUESTION,
           RESEARCH_QUERIES.join("\n"),
         ),
         { input: jsonPayload({ queries: 5 }), output: jsonPayload({ results: 5 }) },
         toolPayload("web_search", { query: RESEARCH_QUERIES[0] }, jsonPayload({ results: [OWASP_CSV] })),
         toolPayload("web_search", { query: RESEARCH_QUERIES[1] }, jsonPayload({ results: [OWASP_CSV] })),
         toolPayload("web_search", { query: RESEARCH_QUERIES[2] }, jsonPayload({ results: [RFC_4180] })),
         toolPayload("web_search", { query: RESEARCH_QUERIES[3] }, jsonPayload({ results: [OWASP_CSV] })),
         toolPayload("web_search", { query: RESEARCH_QUERIES[4] }, jsonPayload({ results: [OWASP_CSV] })),
         { input: jsonPayload({ pages: 2 }), output: jsonPayload({ pages: 2, tokens: 12_400 }) },
         toolPayload(
           "fetch_page",
           { url: OWASP_CSV.url },
           textPayload(
             "Cells that begin with =, +, - or @ can run as formulas when the file is opened in a spreadsheet. Prefix them with a single quote, and quote every cell.",
           ),
         ),
         toolPayload(
           "fetch_page",
           { url: RFC_4180.url },
           textPayload(
             "Fields that contain a line break, a double quote or a comma should be enclosed in double quotes. A double quote inside a field is escaped with a second double quote.",
           ),
         ),
         chatPayload(
           "Answer from the sources only, with numbered citations.",
           RESEARCH_QUESTION,
           RESEARCH_ANSWER,
         ),
         {
           input: jsonPayload({ citations: 2, sources: [OWASP_CSV.url, RFC_4180.url] }),
           output: jsonPayload({ passed: true, unsupported_claims: 0 }),
         },
       ],
     },
   };

   const INVOICE_FILE = "invoice-2026-0412.pdf";
   const INVOICE_OCR = [
     "NORTHWIND SUPPLIES",
     "Invoice INV-2026-0412 · 18 Nov 2026",
     "Desk lamp × 4 .......... 236.00",
     "Office chair × 2 ....... 698.00",
     "Monitor arm × 2 ........ 350.00",
     "Total ................ 1,284.00 EUR",
   ].join("\n");
   const INVOICE_FIELDS = {
     vendor: "Northwind Supplies",
     invoice_number: "INV-2026-0412",
     date: "2026-11-18",
     currency: "EUR",
     line_items: 3,
   };
   const EXTRACT_SYSTEM =
     "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total.";

   const EXTRACTION_SHOWCASE: Showcase = {
     index: 6,
     ageMs: 210_000,
     service: "doc-pipeline",
     tags: ["billing"],
     models: ["claude-sonnet-4-5", "gemini-2.5-flash"],
     scores: [{ name: "schema_valid", source: "check", value: { data_type: "boolean", value: true } }],
     spans: [
       { name: "extract-invoice", kind: "chain", from: 0, to: 7_800 },
       { name: "load_pdf", kind: "tool", parent: 0, from: 30, to: 260 },
       { name: "ocr", kind: "tool", parent: 0, from: 270, to: 2_650 },
       { name: "extract fields", kind: "llm", parent: 0, from: 2_670, to: 3_900, model: "gemini-2.5-flash", provider: "google", inputTokens: 2_900, outputTokens: 180, cost: 0.00132, ttftMs: 240 },
       { name: "validate schema", kind: "guardrail", parent: 0, from: 3_910, to: 3_960 },
       { name: "re-extract fields", kind: "llm", parent: 0, from: 3_980, to: 7_300, model: "claude-sonnet-4-5", provider: "anthropic", inputTokens: 3_100, outputTokens: 260, cost: 0.0132, ttftMs: 520 },
       { name: "validate schema", kind: "guardrail", parent: 0, from: 7_310, to: 7_350 },
       { name: "save_invoice", kind: "tool", parent: 0, from: 7_370, to: 7_760 },
     ],
     story: {
       inputPreview: `${INVOICE_FILE} (2 pages)`,
       outputPreview: "Northwind Supplies · INV-2026-0412 · 1,284.00 EUR",
       payloads: [
         {
           input: jsonPayload({ file: INVOICE_FILE, pages: 2 }),
           output: jsonPayload({ ...INVOICE_FIELDS, total: 1284 }),
         },
         toolPayload("load_pdf", { file: INVOICE_FILE }, jsonPayload({ pages: 2, bytes: 184_320 })),
         toolPayload("ocr", { file: INVOICE_FILE, pages: 2 }, textPayload(INVOICE_OCR)),
         chatPayload(EXTRACT_SYSTEM, INVOICE_OCR, JSON.stringify({ ...INVOICE_FIELDS, total: 1248 })),
         {
           input: jsonPayload({ total: 1248, line_items_sum: 1284 }),
           output: jsonPayload({
             passed: false,
             reasons: ["The total does not match the sum of the line items."],
           }),
         },
         chatPayload(
           EXTRACT_SYSTEM,
           `${INVOICE_OCR}\n\nThe total did not match the line items. Read it again.`,
           JSON.stringify({ ...INVOICE_FIELDS, total: 1284 }),
         ),
         {
           input: jsonPayload({ total: 1284, line_items_sum: 1284 }),
           output: jsonPayload({ passed: true, reasons: [] }),
         },
         toolPayload(
           "save_invoice",
           { invoice_number: "INV-2026-0412" },
           jsonPayload({ id: "inv_8f3a", status: "saved" }),
         ),
       ],
     },
   };

   /** The hand-written traces, by dataset index (M11, M12). Exported for the tests. */
   export const SHOWCASES: readonly Showcase[] = [
     SUPPORT_SHOWCASE,
     CODING_SHOWCASE,
     RESEARCH_SHOWCASE,
     EXTRACTION_SHOWCASE,
   ];

   function showcaseAt(index: number): Showcase | undefined {
     return SHOWCASES.find((showcase) => showcase.index === index);
   }
   ```
   Check the numbers by hand before going on. Every child's `[from, to]` lies inside its parent's `[from, to]`, and every parent comes before its children; E1's tests check both.

4. **Summary.**
   - In `traceSummary`, replace the `SHOWCASE_INDEX` check with:
     ```ts
     const showcase = showcaseAt(index);
     if (showcase) {
       return showcaseSummary(showcase, index, now);
     }
     ```
   - Change `showcaseSummary` to `showcaseSummary(showcase: Showcase, index: number, now: number): Schema<"TraceSummary">`. It computes what it computes today, from `showcase` instead of the constants:
     - `name` is `showcase.spans[0].name`.
     - The duration is `showcase.spans[0].to`.
     - The age is `showcase.ageMs`.
     - The token and cost sums run over `showcase.spans`.
     - `models` is `showcase.models` and `tags` is `showcase.tags`.
     - `scores` is `showcase.scores`, mapped to `{ name, score_id: scoreUuid(index, i), source, value }` with `i` its position.
     - `error_count` is the number of spans with `status === "error"`, and `status` is `"error"` when that count is above 0, else `"ok"`.
     - `metadata` is `showcase.user ? userTraceMetadata(showcase.user) : {}`.
     - `user_id` is set only when `showcase.user` is set, using `...(showcase.user ? { user_id: showcase.user } : {})`.
     - `environment` stays `"production"`, `release` `"v1.5.0"`, and there is still no `session_id`.
   - Update its doc comment: the sums run over `showcase.spans`.
   - The new score names (`tests_pass`, `groundedness`, `schema_valid`) have no entry in `SCORE_DEFINITIONS` (`web/src/mocks/handlers.ts`), so they show without a tone colour. That is accepted: don't add definitions (`handlers.test.ts` pins the list).

5. **Previews.** In `createDataset`, change the line that builds `traces` to the code below. `timed` keeps the indices of `bases`; check that `spreadSessionTurns` returns a copy in the same order.
   ```ts
   const traces = timed.map((trace, index) => {
     const story = showcaseAt(index)?.story;
     return story
       ? { ...trace, input_preview: story.inputPreview, output_preview: story.outputPreview }
       : withPreviews(trace, turnAt(index));
   });
   ```

6. **Tree.**
   - In `buildDetail`, replace the `SHOWCASE_INDEX` check with the same `showcaseAt` pattern, calling `showcaseDetail(showcase, index, trace)`.
   - In `showcaseDetail`, read `showcase.spans` instead of `SHOWCASE_SPANS`.
   - Set `service_name: showcase.service` and `status: row.status ?? "ok"`, and add `...(row.statusMessage ? { status_message: row.statusMessage } : {})` (generic error spans carry `status_message: "tool failed"` the same way).
   - Update its doc comment: the spans come in `showcase.spans` order.
   - Nothing else changes.

7. **Payloads.**
   - Rename `applyShowcasePayload` to `applySupportPayload`, delete its `detail.truncated = [];` line and its comment, and replace `SHOWCASE_USER` in it with `"user-012"`.
   - At the end of `spanFromSummary`, replace the `SHOWCASE_INDEX` block with:
     ```ts
     const showcase = showcaseAt(context.traceIndex);
     if (showcase) {
       // The generic rule flags every 17th span's input, the root included; the hero shows no truncation badge.
       detail.truncated = [];
       if (showcase.story) {
         applyStoryPayload(detail, showcase.story.payloads[spanIndex(detail.span_id)]);
       } else {
         applySupportPayload(detail, context.traceIndex, context.turn.exchange.question);
       }
     }
     ```
   - Write `applyStoryPayload(detail: Schema<"SpanDetail">, payload: ShowcasePayload | undefined): void`:
     1. If `payload` is undefined, return. No span of a story lacks one; a test checks that.
     2. Set `detail.input = payload.input` and `detail.output = payload.output`.
     3. If `payload.error` is set, set `detail.events` to one event:
        ```ts
        {
          name: "exception",
          time: detail.end_time,
          attributes: {
            "exception.type": payload.error.type,
            "exception.message": payload.error.message,
            "exception.stacktrace": payload.error.stacktrace,
          },
        }
        ```
        This replaces the generic one that `spanFromSummary` adds for error spans.

8. **No leftovers.** `rtk proxy grep -n "SHOWCASE_INDEX\|SHOWCASE_SPANS\|SHOWCASE_USER\|SHOWCASE_AGE_MS\|SHOWCASE_DURATION_MS" web/src` prints nothing.

9. **Tests.** Edit `dataset.test.ts`. Import `SHOWCASES` from `@/mocks/generated/dataset`. Use `createDataset({ seed: 1, now: NOW, traces: 10 })` unless a test says otherwise.
   - **Change** "gives a trace without a session a topic of its own": filter out the story traces before taking five. Change `.filter((trace) => !trace.session_id)` to `.filter((trace, i) => !trace.session_id && !SHOWCASES.some((showcase) => showcase.index === i + 1 && showcase.story))`. The `i + 1` accounts for the `slice(1)`. The test covers generated traces only.
   - **Keep** the five support-agent tests unchanged. They must still pass.
   - **Add** `"makes traces 4 to 6 the hand-written scenario traces"`. For each of indices 4, 5 and 6, compare the trace with `SHOWCASES.find((s) => s.index === i)`:
     - its `name` is the root span's name (`coding-agent`, `research-agent`, `extract-invoice`), and it has no `session_id`;
     - `span_count` equals the detail's span count;
     - `llm_count` equals the number of llm spans;
     - `input_tokens`, `output_tokens` and `cost_total` equal the sums over the detail's non-root spans (cost to 6 decimals);
     - `input_preview` and `output_preview` equal the story's;
     - it starts `ageMs` before `NOW`, and lasts the root's `to` in ms.
   - **Add** `"nests every span of every showcase inside its parent, in pre-order"`. For indices 3 to 6: every span with a parent comes after it, its `depth` is the parent's + 1, and its start and end lie within the parent's.
   - **Add** `"gives every span of a scenario its hand-written payloads"`. For each showcase with a `story`:
     - `story.payloads.length` equals `spans.length`;
     - for every span, `dataset.spanDetail(traceId, spanId)` has `input` and `output` deep-equal to `story.payloads[position]`, and `truncated` equal to `[]`.
   - **Add** `"fails the coding agent's first test run"`:
     - trace 4 has `status` `"error"` and `error_count` 1;
     - its span at position 8 (`run_tests`) has status `"error"`, `status_message` `"1 failed, 47 passed"`, and exactly one event, named `exception`, with `expect(event.attributes).toMatchObject({ "exception.message": "1 failed, 47 passed" })` (`attributes` is typed `unknown`, so don't index it);
     - the span at position 12 (`run_tests`) has status `"ok"` and no event.
   - Rows taken by position (`spans[8]`, `spans[12]`, …) can be `undefined` to the type checker: guard them as the existing showcase tests do (`if (!row) throw new Error(...)`).
   - The other existing tests (those with `traces: 3`, `traces: 4`, and the session tests) stay valid unchanged: the showcases have no `session_id`, and `traces: 4` ends at index 3. Don't edit them.
   - **Add** `"runs the research agent's five searches in parallel"`: trace 5's five `web_search` spans share one `start_time`.
   - **Add** `"fails the invoice's first schema check and passes the second"`: trace 6's span at position 4 has output `passed: false`, and the one at position 6 has `passed: true`.

**Done when:**
- From the worktree root, after `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`, these pass with no errors:
  - `pnpm --dir web exec vitest run src/mocks 'src/routes/$org/$project/traces' src/components/trace src/test/quality-bar.test.tsx --maxWorkers=8`
  - `pnpm --dir web run typecheck`
  - `./node_modules/.bin/biome check web/src/mocks/generated/dataset.ts web/src/mocks/generated/dataset.test.ts`
- If a test outside `dataset.test.ts` fails because it relied on the generated content of traces 4–6, stop and report it with `QUESTION:`. Don't edit that test.
- `just check-web` runs once, at the end of E3.

**Commit:** `Add three hand-written scenario traces to the mock dataset`

### E2 Scenario screenshots

**Goal:** each new trace has a hero screenshot, and every screenshot reflects the new dataset.

**Edit:** `website/screenshots.json` and the PNGs `just docs-screenshots` rewrites under `website/src/assets/screenshots/`. No other file.

**Steps:**
1. In `website/screenshots.json`, add three entries right after `marketing-trace-view`.
   - Each is a copy of it: same `path`, `waitFor`, `theme`, `viewport`, the Timeline action, `waitAfter` and `clip`.
   - Only `name` and the first action's text change:

   | `name` | First action |
   |---|---|
   | `marketing-trace-view-coding` | `div[role="grid"][aria-label="Traces"] >> text=coding-agent` |
   | `marketing-trace-view-research` | `div[role="grid"][aria-label="Traces"] >> text=research-agent` |
   | `marketing-trace-view-extraction` | `div[role="grid"][aria-label="Traces"] >> text=extract-invoice` |

   In the JSON, the quotes inside the selector are escaped, as in the existing entry.
2. Run `just docs-screenshots` for every shot: the dataset change moves the trace list, the sessions, the users and the dashboards. Set `CETRA_SCREENSHOTS_PORT=5299` if port 5199 is busy.
3. Look with the Read tool at the three new PNGs, at `marketing-trace-view.png`, and at every other PNG that changed (`git status`). The three new PNGs must:
   - be 2398 px wide, the same size as `marketing-trace-view.png`;
   - show their trace's spans by name, in the Timeline view, with the root span selected and its Input / Output on the right;
   - show no truncation badge;
   - show `run_tests` in red, in the coding agent's shot.

   `docs-screenshots` rewrites every PNG and anti-aliasing alone can change the bytes, so `git status` lists most shots: judge them by looking, not by the list. The other shots may change only in rows, counts and charts. Report any that changed another way: a different page, an error or an empty state. The commit may touch about 25 PNGs.
4. If a new shot cuts off its root's Output card at the bottom, report it with `QUESTION:`. Don't change the viewport: all four hero shots share one size.

**Done when:** `just check-docs` passes.

**Commit:** `Capture a hero screenshot of each scenario trace`

### E3 Hero tabs

**Goal:** the hero shows the four scenario screenshots behind labelled tabs, each with a caption.

**Edit:** `website/src/marketing/home/Hero.astro`; and `docs/plans/website/README.md`, only to set the M12 row's status to `done (E1–E3)`. No other file.

**Steps:**
1. In `Hero.astro`'s frontmatter:
   - Import the three new PNGs next to `traceView`, as `traceCoding`, `traceResearch` and `traceExtraction`.
   - Import `Tabs from "../components/Tabs.astro"`.
   - Add this array, exactly:
   ```ts
   const SCENARIOS = [
     {
       tab: "Support agent",
       src: traceView,
       label: "Traces · Support bot",
       alt: "The cetra trace view: a support agent's run, with its span tree, timeline, models, tokens and costs, and the root span's input and output.",
       caption: "Retrieval, two tools in parallel, a PII check and an LLM judge, with the cost of every call.",
     },
     {
       tab: "Coding agent",
       src: traceCoding,
       label: "Traces · Code agent",
       alt: "The cetra trace view: a coding agent's run, where the first test run fails and the second passes after a fix, with its timeline, models, tokens and costs.",
       caption: "The first test run fails. The agent reads the error, fixes the test, and the second run passes.",
     },
     {
       tab: "Research agent",
       src: traceResearch,
       label: "Traces · Research",
       alt: "The cetra trace view: a research agent's run, with five web searches and two page reads in parallel, then one long call that writes a cited answer.",
       caption: "Five searches and two page reads run in parallel, then one long call writes the cited answer.",
     },
     {
       tab: "Document extraction",
       src: traceExtraction,
       label: "Traces · Document pipeline",
       alt: "The cetra trace view: an invoice extraction pipeline, where a small model's answer fails the schema check and a larger model reads the invoice again.",
       caption: "A small model reads the invoice. The schema check catches a wrong total, so a larger model reads it again.",
     },
   ];
   ```
2. Replace the content of `<div class="visual">` with:
   ```astro
   <Tabs id="hero-scenarios" label="Example traces" labels={SCENARIOS.map((scenario) => scenario.tab)}>
     {SCENARIOS.map((scenario, i) => (
       <div data-tab-panel hidden={i !== 0}>
         <Frame src={scenario.src} alt={scenario.alt} label={scenario.label} fade={false} eager={i === 0} />
         <p class="caption">{scenario.caption}</p>
       </div>
     ))}
   </Tabs>
   ```
   The server renders panels 2–4 hidden, so the page doesn't show four pictures before the script runs. Frames 2–4 are lazy (`eager={false}`), so their pictures load when their tab is opened.
3. Add to the `<style>` block:
   ```css
   .visual :global(.tablist) {
     justify-content: safe center;
     border-bottom: 0;
     margin-bottom: var(--space-4);
   }
   .caption {
     margin-top: var(--space-3);
     text-align: center;
     font-size: var(--text-sm);
     color: var(--fg-muted);
   }
   ```
   `Tabs.astro` sets `overflow-x: auto` on the tab list, so on phones the four tabs scroll sideways instead of overflowing the page. `safe center` keeps the first tab reachable when the list overflows; plain `center` would clip it on the left.
4. Build the site (`pnpm --dir website exec astro build`), then run the `pageshots.mjs` tool from the README conventions on `/`. At 1440 px:
   - the four tabs are centred above the picture;
   - "Support agent" is selected;
   - the caption sits under the frame;
   - there is no `HORIZONTAL OVERFLOW` at either width.
5. Check the tab switching in a real browser. Write a scratch Playwright script in the scratchpad, not in the repository:
   1. Serve the build with `astro preview` on port 4329.
   2. Click "Coding agent".
   3. Check that its panel is visible, the other three are hidden, and its `img` has loaded (`naturalWidth > 0`).
   4. Check that, before the click, the coding picture's `img` had not loaded (`complete` false or `naturalWidth` 0).
   5. At a 390 px wide viewport, check that the "Support agent" tab's left edge is not left of the tab list's left edge (`getBoundingClientRect()`), so the first tab is fully visible.
   6. Stop the server.

   Report what you saw.
6. Set the M12 row's status in `docs/plans/website/README.md` to `done (E1–E3)`.

**Done when:** `just check-web` and `just check-docs` pass, and steps 4–5 show what they describe.

**Commit:** `Show four scenario traces behind tabs in the hero`
