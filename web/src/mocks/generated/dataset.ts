import type { Schema } from "@/api/types";
import { mulberry32 } from "@/mocks/generated/random";

/** Re-exported for the story modules (M17). */
export { mulberry32 };

const DAY_MS = 86_400_000;
const NAMES = [
  "support-router",
  "refund-agent",
  "ticket-summary",
  "billing-lookup",
  "handoff",
  "retrieval",
  "guardrail-check",
  "draft-reply",
  "escalate",
  "close-ticket",
  "qa-review",
  "memory-update",
] as const;
const MODELS = [
  "claude-sonnet-4-5",
  "claude-haiku-4-5",
  "gpt-4.1",
  "gpt-4.1-mini",
  "gemini-2.5-pro",
  "gemini-2.5-flash",
] as const;
const TAGS = ["billing", "priority", "voice", "email", "retry"] as const;
/** Generated voice calls last 0.4 to 4 s, not the 0.2 to 20 s of the other traces (M14). */
const VOICE_MIN_MS = 400;
const VOICE_SPAN_MS = 3_600;
/** The drawn duration is `200 + floor(rng() * 20_000)`: 200 to 20,199 ms. */
const DRAWN_MIN_MS = 200;
const DRAWN_SPAN_MS = 20_000;
const USER_PLANS = ["free", "pro", "team"] as const;
const USER_COUNTRIES = ["FR", "US", "DE", "JP"] as const;
const KINDS = [
  "llm",
  "tool",
  "chain",
  "llm",
  "retriever",
  "span",
  "embedding",
  "guardrail",
  "evaluator",
  "reranker",
] as const satisfies readonly Schema<"SpanKind">[];
const BUCKET_MS: Record<Exclude<Schema<"MetricsBucket">, "auto">, number> = {
  "1m": 60_000,
  "5m": 300_000,
  "10m": 600_000,
  "15m": 900_000,
  "30m": 1_800_000,
  "1h": 3_600_000,
  "6h": 21_600_000,
  "12h": 43_200_000,
  "1d": DAY_MS,
};

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
export type Showcase = {
  index: number;
  ageMs: number;
  service: string;
  user?: string;
  /** The showcase's session id (M17). */
  session?: string;
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

const SUPPORT_SPANS: readonly [ShowcaseSpan, ...ShowcaseSpan[]] = [
  { name: "support-agent", kind: "agent", from: 0, to: 6_200 },
  { name: "prompt-injection check", kind: "guardrail", parent: 0, from: 20, to: 140 },
  {
    name: "classify intent",
    kind: "llm",
    parent: 0,
    from: 150,
    to: 560,
    model: "claude-haiku-4-5",
    provider: "anthropic",
    inputTokens: 412,
    outputTokens: 9,
    cost: 0.000457,
    ttftMs: 310,
  },
  { name: "retrieve docs", kind: "retriever", parent: 0, from: 580, to: 1_480 },
  {
    name: "embed query",
    kind: "embedding",
    parent: 3,
    from: 590,
    to: 760,
    model: "text-embedding-3-small",
    provider: "openai",
  },
  { name: "vector search", kind: "retriever", parent: 3, from: 770, to: 1_150 },
  {
    name: "rerank",
    kind: "reranker",
    parent: 3,
    from: 1_160,
    to: 1_470,
    model: "rerank-v3.5",
    provider: "cohere",
  },
  {
    name: "plan",
    kind: "llm",
    parent: 0,
    from: 1_500,
    to: 2_600,
    model: "claude-sonnet-4-5",
    provider: "anthropic",
    inputTokens: 1_840,
    outputTokens: 156,
    cost: 0.00786,
    ttftMs: 420,
  },
  { name: "get_account", kind: "tool", parent: 0, from: 2_620, to: 2_910 },
  { name: "check_export_limits", kind: "tool", parent: 0, from: 2_620, to: 3_230 },
  {
    name: "draft answer",
    kind: "llm",
    parent: 0,
    from: 3_250,
    to: 5_560,
    model: "claude-sonnet-4-5",
    provider: "anthropic",
    inputTokens: 2_310,
    outputTokens: 248,
    cost: 0.01065,
    ttftMs: 380,
    prompt: { name: "support-reply", version: 2 },
  },
  { name: "pii scrub", kind: "guardrail", parent: 0, from: 5_570, to: 5_650 },
  {
    name: "judge helpfulness",
    kind: "evaluator",
    parent: 0,
    from: 5_660,
    to: 6_190,
    model: "gpt-4.1-mini",
    provider: "openai",
    inputTokens: 690,
    outputTokens: 42,
    cost: 0.000343,
  },
];

export const SUPPORT_SHOWCASE: Showcase = {
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

export const CODING_SHOWCASE: Showcase = {
  index: 4,
  ageMs: 150_000,
  service: "code-agent",
  user: "user-007",
  tags: ["retry"],
  models: ["claude-sonnet-4-5", "claude-haiku-4-5"],
  scores: [{ name: "tests_pass", source: "check", value: { data_type: "boolean", value: true } }],
  spans: [
    { name: "coding-agent", kind: "agent", from: 0, to: 18_400 },
    {
      name: "plan",
      kind: "llm",
      parent: 0,
      from: 120,
      to: 2_300,
      model: "claude-sonnet-4-5",
      provider: "anthropic",
      inputTokens: 3_120,
      outputTokens: 210,
      cost: 0.01251,
      ttftMs: 520,
    },
    { name: "gather context", kind: "chain", parent: 0, from: 2_330, to: 2_640 },
    { name: "read_file", kind: "tool", parent: 2, from: 2_340, to: 2_410 },
    { name: "search_code", kind: "tool", parent: 2, from: 2_340, to: 2_630 },
    { name: "attempt 1", kind: "chain", parent: 0, from: 2_660, to: 10_240 },
    {
      name: "write patch",
      kind: "llm",
      parent: 5,
      from: 2_670,
      to: 6_900,
      model: "claude-sonnet-4-5",
      provider: "anthropic",
      inputTokens: 5_400,
      outputTokens: 620,
      cost: 0.0255,
      ttftMs: 610,
    },
    { name: "apply_patch", kind: "tool", parent: 5, from: 6_920, to: 7_010 },
    {
      name: "run_tests",
      kind: "tool",
      parent: 5,
      from: 7_030,
      to: 10_230,
      status: "error",
      statusMessage: "1 failed, 47 passed",
    },
    { name: "attempt 2", kind: "chain", parent: 0, from: 10_260, to: 17_060 },
    {
      name: "fix failing test",
      kind: "llm",
      parent: 9,
      from: 10_270,
      to: 13_700,
      model: "claude-sonnet-4-5",
      provider: "anthropic",
      inputTokens: 7_100,
      outputTokens: 380,
      cost: 0.027,
      ttftMs: 640,
    },
    { name: "apply_patch", kind: "tool", parent: 9, from: 13_720, to: 13_790 },
    { name: "run_tests", kind: "tool", parent: 9, from: 13_810, to: 17_050 },
    {
      name: "summarize",
      kind: "llm",
      parent: 0,
      from: 17_080,
      to: 18_350,
      model: "claude-haiku-4-5",
      provider: "anthropic",
      inputTokens: 1_900,
      outputTokens: 120,
      cost: 0.0025,
      ttftMs: 280,
    },
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
          ["def escape_field(value: str) -> str:", "    return value.replace('\"', '\"\"')"].join(
            "\n",
          ),
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
      toolPayload(
        "apply_patch",
        { path: "src/export/csv.py" },
        jsonPayload({ applied: true, hunks: 1 }),
      ),
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
          '+    assert escape_field("a,b") == \'"a,b"\'',
          "```",
        ].join("\n"),
      ),
      toolPayload(
        "apply_patch",
        { path: "tests/test_csv.py" },
        jsonPayload({ applied: true, hunks: 1 }),
      ),
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

const VOICE_QUESTION = "Hi, can I move my dentist appointment to Friday afternoon?";
const VOICE_ANSWER = "Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you?";

export const VOICE_SHOWCASE: Showcase = {
  index: 5,
  ageMs: 180_000,
  service: "voice-agent",
  user: "user-019",
  tags: ["voice"],
  models: ["gpt-4.1-mini", "gpt-4o-mini-transcribe", "gpt-4o-mini-tts"],
  scores: [
    { name: "under_budget", source: "check", value: { data_type: "boolean", value: false } },
  ],
  spans: [
    { name: "voice-agent", kind: "agent", from: 0, to: 3_100 },
    { name: "detect end of turn", kind: "span", parent: 0, from: 0, to: 120 },
    {
      name: "speech to text",
      kind: "llm",
      parent: 0,
      from: 130,
      to: 520,
      model: "gpt-4o-mini-transcribe",
      provider: "openai",
      inputTokens: 310,
      outputTokens: 22,
      cost: 0.0003,
    },
    {
      name: "detect intent",
      kind: "llm",
      parent: 0,
      from: 530,
      to: 880,
      model: "gpt-4.1-mini",
      provider: "openai",
      inputTokens: 610,
      outputTokens: 14,
      cost: 0.00027,
      ttftMs: 210,
    },
    { name: "check_availability", kind: "tool", parent: 0, from: 890, to: 2_110 },
    {
      name: "draft reply",
      kind: "llm",
      parent: 0,
      from: 2_120,
      to: 2_640,
      model: "gpt-4.1-mini",
      provider: "openai",
      inputTokens: 980,
      outputTokens: 46,
      cost: 0.00047,
      ttftMs: 180,
    },
    { name: "safety check", kind: "guardrail", parent: 0, from: 2_650, to: 2_700 },
    {
      name: "text to speech",
      kind: "llm",
      parent: 0,
      from: 2_710,
      to: 3_100,
      model: "gpt-4o-mini-tts",
      provider: "openai",
      inputTokens: 46,
      outputTokens: 820,
      cost: 0.0098,
      ttftMs: 90,
    },
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

export const EXTRACTION_SHOWCASE: Showcase = {
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
    {
      name: "extract fields",
      kind: "llm",
      parent: 0,
      from: 2_670,
      to: 3_900,
      model: "gemini-2.5-flash",
      provider: "google",
      inputTokens: 2_900,
      outputTokens: 180,
      cost: 0.00132,
      ttftMs: 240,
    },
    { name: "validate schema", kind: "guardrail", parent: 0, from: 3_910, to: 3_960 },
    {
      name: "re-extract fields",
      kind: "llm",
      parent: 0,
      from: 3_980,
      to: 7_300,
      model: "claude-sonnet-4-5",
      provider: "anthropic",
      inputTokens: 3_100,
      outputTokens: 260,
      cost: 0.0132,
      ttftMs: 520,
    },
    { name: "validate schema", kind: "guardrail", parent: 0, from: 7_310, to: 7_350 },
    { name: "save_invoice", kind: "tool", parent: 0, from: 7_370, to: 7_760 },
  ],
  story: {
    inputPreview: `${INVOICE_FILE} (2 pages)`,
    outputPreview: "Northwind Supplies · INV-2026-0412 · 18 Nov 2026 · 3 line items · 1,284.00 EUR",
    payloads: [
      {
        input: textPayload(`${INVOICE_FILE} (2 pages)`),
        output: textPayload(
          "Northwind Supplies · INV-2026-0412 · 18 Nov 2026 · 3 line items · 1,284.00 EUR",
        ),
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
  VOICE_SHOWCASE,
  EXTRACTION_SHOWCASE,
];

function showcaseAt(index: number, profile: Profile): Showcase | undefined {
  return profile.showcases.find((showcase) => showcase.index === index);
}

type BuiltSpan = {
  depth: number;
  detached: boolean;
  parent?: number;
  startMs: number;
  endMs: number;
  kind: Schema<"SpanKind">;
  status: Schema<"SpanStatus">;
};

export type Dataset = {
  /** The `now` the traces were generated against; generated datasets (`datasets.ts`) share it. */
  now: number;
  traces: Schema<"TraceSummary">[];
  traceDetail(traceId: string): Schema<"TraceDetail"> | undefined;
  spanDetail(traceId: string, spanId: string): Schema<"SpanDetail"> | undefined;
  sessions(includeEval: boolean, includeExperiments: boolean): Schema<"SessionSummary">[];
  session(
    sessionId: string,
    includeEval: boolean,
    includeExperiments: boolean,
  ): Schema<"SessionDetail"> | undefined;
  /** Users aggregated over their traces that start in `[window.from, window.to)`, as `user` does. */
  users(
    includeEval: boolean,
    includeExperiments: boolean,
    window: { from?: number; to?: number },
  ): UserGroup[];
  /** The user's traces that start in `[window.from, window.to)`; an absent bound is open (UP-1). */
  user(
    userId: string,
    includeEval: boolean,
    includeExperiments: boolean,
    window: { from?: number; to?: number },
  ): Schema<"EndUserDetail"> | undefined;
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
  /** The trace's name, and its root span's: the trace summary and the root of its detail show it. */
  name?: string;
  /** Replace the trace's scores with the same name, and add the others. */
  scores?: readonly { name: string; source: Schema<"ScoreSource">; value: Schema<"ScoreValue"> }[];
  /** The trace's turn (its previews and its generated LLM payloads) instead of the drawn one. */
  turn?: Turn;
  /** The trace becomes an error trace: `count` error spans, named `spanName`, with this message. */
  error?: { count: number; spanName: string; message: string; stacktrace: string };
  /** Input and output tokens are multiplied by it (rounded down), so the cost is too. */
  costFactor?: number;
  /** A new start time, epoch ms; the end moves with it. */
  startMs?: number;
  /** A new duration; marked spans that end later extend it to their end. */
  durationMs?: number;
  spans?: readonly MarkedSpan[];
};

export type MarksContext = {
  seed: number;
  now: number;
  /**
   * True for the traces a mark may choose: not a showcase, not the big first trace, and visible with the
   * trace list's defaults (environment not `cetra-eval`, no `cetra.experiment_id` metadata). A mark for
   * another index is ignored.
   */
  eligible: (index: number) => boolean;
};

/** Every table and range of a generated dataset's traces, sessions and users (M16). */
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
  tool: (traceIndex: number) => {
    arguments: Record<string, unknown>;
    result: Record<string, unknown>;
  };
  systemPrompt: string;
  failedAnswer: string;
  /** Scores of a generated trace. */
  scores: (index: number, errored: boolean) => Schema<"ScoreBrief">[];
  showcases: readonly Showcase[];
  /**
   * More metadata of a generated trace, from its index and its age in ms; absent: none. It draws nothing from the
   * trace's generator, so adding it moves no other value.
   */
  traceMetadata?: (index: number, ageMs: number) => Record<string, string>;
  /** Changes for chosen generated traces, by index; absent: none (M17). */
  marks?: (
    traces: readonly Schema<"TraceSummary">[],
    context: MarksContext,
  ) => ReadonlyMap<number, Mark>;
};

/** The session and user a `calls` profile gives a generated trace. */
type CallIds = { session: string; user?: string };

/** Salt of the call sizes, so they differ from every other draw of the generator. */
const CALL_SALT = 0xbb67ae85;

/**
 * The calls of a profile with `sessions.kind === "calls"`, by trace index: runs of `min` to `max` consecutive
 * generated traces (showcase indices are skipped). The last call keeps the traces it has, even fewer than `min`.
 */
function callsOf(seed: number, traces: number, profile: Profile): Map<number, CallIds> {
  const calls = new Map<number, CallIds>();
  if (profile.sessions.kind !== "calls") {
    return calls;
  }
  const { min, max, prefix } = profile.sessions;
  const showcases = new Set(profile.showcases.map((showcase) => showcase.index));
  const indices: number[] = [];
  for (let index = 0; index < traces; index += 1) {
    if (!showcases.has(index)) {
      indices.push(index);
    }
  }
  let position = 0;
  for (let call = 0; position < indices.length; call += 1) {
    const random = mulberry32(mix(seed ^ CALL_SALT, call));
    const size = min + Math.floor(random() * (max - min + 1));
    const ids: CallIds = { session: `${prefix}${String(call).padStart(4, "0")}` };
    if (profile.users.kind === "per-call") {
      ids.user = `${profile.users.prefix}${String(call % profile.users.pool).padStart(3, "0")}`;
    }
    for (const index of indices.slice(position, position + size)) {
      calls.set(index, ids);
    }
    position += size;
  }
  return calls;
}

export function createDataset(options: {
  seed: number;
  now: number;
  traces: number;
  /** The generation profile; `SUPPORT_PROFILE` when absent. */
  profile?: Profile;
}): Dataset {
  const profile = options.profile ?? SUPPORT_PROFILE;
  const calls = callsOf(options.seed, options.traces, profile);
  const bases: Schema<"TraceSummary">[] = [];
  for (let index = 0; index < options.traces; index += 1) {
    bases.push(traceSummary(options.seed, index, options.now, profile, calls.get(index)));
  }
  // A session's turns are spread out in time first: the session's turns depend on each other.
  const timed = spreadSessionTurns(bases, options.seed, options.now, profile);
  // The marks change the timed traces before the sessions are numbered, since the numbering follows start times.
  const marks = marksOf(timed, options.seed, options.now, profile);
  const marked = timed.map((trace, index) => {
    const mark = marks.get(index);
    return mark ? markedSummary(trace, index, mark, profile) : trace;
  });
  // A trace's turn depends on its session's other traces, so the previews are set once all are known.
  const slots = sessionSlots(marked);
  const turnAt = (index: number): TraceTurn => {
    const markedTurn = marks.get(index)?.turn;
    if (markedTurn) {
      return { topic: markedTurn, exchange: markedTurn, main: true };
    }
    const slot = slots.get(index);
    return turnOf(slot?.topic ?? index, slot?.turn ?? 1, profile);
  };
  const traces = marked.map((trace, index) => {
    const story = showcaseAt(index, profile)?.story;
    return story
      ? { ...trace, input_preview: story.inputPreview, output_preview: story.outputPreview }
      : withPreviews(trace, turnAt(index), profile);
  });
  const indexById = new Map<string, number>();
  for (const [index, trace] of traces.entries()) {
    indexById.set(trace.trace_id, index);
  }
  const details = new Map<string, Schema<"TraceDetail">>();

  function traceDetail(traceId: string): Schema<"TraceDetail"> | undefined {
    const index = indexById.get(traceId);
    const trace = index === undefined ? undefined : traces[index];
    if (index === undefined || !trace) {
      return undefined;
    }
    const cached = details.get(traceId);
    if (cached) {
      return cached;
    }
    const detail = buildDetail(options.seed, index, trace, profile, marks.get(index));
    details.set(traceId, detail);
    return detail;
  }

  return {
    now: options.now,
    traces,
    traceDetail,
    spanDetail(traceId, spanId) {
      const detail = traceDetail(traceId);
      const row = detail?.spans.find((span) => span.span.span_id === spanId);
      if (!detail || !row) {
        return undefined;
      }
      const index = indexById.get(traceId) ?? 0;
      const mark = marks.get(index);
      return spanFromSummary(row.span, {
        traceIndex: index,
        errored: detail.trace.error_count > 0,
        turn: turnAt(index),
        profile,
        mark,
        generatedCount: detail.trace.span_count - markedSpanCount(mark),
      });
    },
    sessions(includeEval, includeExperiments) {
      return sessionsFrom(visibleTraces(traces, includeEval, includeExperiments));
    },
    session(sessionId, includeEval, includeExperiments) {
      const rows = visibleTraces(traces, includeEval, includeExperiments)
        .filter((trace) => trace.session_id === sessionId)
        .sort((left, right) => left.start_time.localeCompare(right.start_time));
      const session = sessionsFrom(rows).find((item) => item.session_id === sessionId);
      if (!session) {
        return undefined;
      }
      const turns = rows.slice(0, 1000).map((trace) => ({
        trace,
        input: textPayload(trace.input_preview ?? ""),
        output: textPayload(trace.output_preview ?? ""),
      }));
      return { session, turns, truncated: rows.length > 1000 };
    },
    users(includeEval, includeExperiments, window) {
      return usersFrom(
        visibleTraces(traces, includeEval, includeExperiments).filter((trace) =>
          startsIn(trace.start_time, window),
        ),
      );
    },
    user(userId, includeEval, includeExperiments, window) {
      const rows = visibleTraces(traces, includeEval, includeExperiments).filter(
        (trace) => trace.user_id === userId && startsIn(trace.start_time, window),
      );
      const group = usersFrom(rows).find((item) => item.user.user_id === userId);
      if (!group) {
        return undefined;
      }
      const activityWindow =
        window.from !== undefined && window.to !== undefined
          ? { from: window.from, to: window.to }
          : undefined;
      const user = userSummaries([group], activityWindow)[0];
      if (!user) {
        return undefined;
      }
      const sessions = sessionsFrom(rows).sort((left, right) =>
        right.end_time.localeCompare(left.end_time),
      );
      return { user, sessions: sessions.slice(0, 100) };
    },
  };
}

/** Whether `start` is in `[window.from, window.to)`; an absent bound is open. */
function startsIn(start: string, window: { from?: number; to?: number }): boolean {
  const time = Date.parse(start);
  return (
    (window.from === undefined || time >= window.from) &&
    (window.to === undefined || time < window.to)
  );
}

export function visibleTraces(
  traces: readonly Schema<"TraceSummary">[],
  includeEval: boolean,
  includeExperiments: boolean,
): Schema<"TraceSummary">[] {
  return traces.filter((trace) => {
    if (!includeEval && trace.environment === "cetra-eval") {
      return false;
    }
    if (!includeExperiments && trace.metadata["cetra.experiment_id"] !== undefined) {
      return false;
    }
    return true;
  });
}

/** The number of spans a mark adds under the root (M17). */
function markedSpanCount(mark: Mark | undefined): number {
  return mark?.spans?.length ?? 0;
}

/**
 * Whether a marks hook may choose trace `index` of a profile's dataset (M17): not a showcase, not the big first
 * trace, and visible with the trace list's defaults. Facts and marks both use this rule.
 */
export function isEligibleTrace(
  profile: Profile,
  trace: Schema<"TraceSummary">,
  index: number,
): boolean {
  return (
    showcaseAt(index, profile) === undefined &&
    !(profile.bigFirstTrace && index === 0) &&
    visibleTraces([trace], false, false).length === 1
  );
}

/**
 * The traces an even spread picks from `items` in order: rank `r` (from 0) is picked when
 * `floor((r + 1) p) > floor(r p)`, so `round(p × n)` of the `n` items, give or take one, evenly along the order (M17).
 */
export function evenSpread<T>(items: readonly T[], p: number): T[] {
  return items.filter((_, rank) => Math.floor((rank + 1) * p) > Math.floor(rank * p));
}

/**
 * Merges a change into a trace's mark (M17): the later fields win; tags, spans and scores concatenate, except that
 * a score of `a` with the name of a score of `b` is replaced. Empty lists are left out.
 */
export function mergeMarks(a: Mark, b: Mark): Mark {
  const tags = [...(a.tags ?? []), ...(b.tags ?? [])];
  const spans = [...(a.spans ?? []), ...(b.spans ?? [])];
  const scores = [
    ...(a.scores ?? []).filter((score) => !b.scores?.some((other) => other.name === score.name)),
    ...(b.scores ?? []),
  ];
  return {
    ...a,
    ...b,
    ...(tags.length > 0 ? { tags } : {}),
    ...(spans.length > 0 ? { spans } : {}),
    ...(scores.length > 0 ? { scores } : {}),
  };
}

/**
 * The marks of a profile, for its eligible traces only (M17). The profile's `marks` sees every timed summary; a
 * mark for an ineligible index is dropped.
 */
function marksOf(
  traces: readonly Schema<"TraceSummary">[],
  seed: number,
  now: number,
  profile: Profile,
): ReadonlyMap<number, Mark> {
  const result = new Map<number, Mark>();
  if (!profile.marks) {
    return result;
  }
  const eligible = (index: number): boolean => {
    const trace = traces[index];
    return trace !== undefined && isEligibleTrace(profile, trace, index);
  };
  for (const [index, mark] of profile.marks(traces, { seed, now, eligible })) {
    if (eligible(index)) {
      result.set(index, mark);
    }
  }
  return result;
}

/** A generated trace's summary after its mark (M17). Its turn and its spans' payloads are applied elsewhere. */
function markedSummary(
  trace: Schema<"TraceSummary">,
  index: number,
  mark: Mark,
  profile: Profile,
): Schema<"TraceSummary"> {
  const marked = mark.spans ?? [];
  const startMs = mark.startMs ?? Date.parse(trace.start_time);
  let durationMs = mark.durationMs ?? trace.duration_ns / 1_000_000;
  for (const span of marked) {
    durationMs = Math.max(durationMs, span.fromMs + span.durationMs);
  }
  // A mark's error count replaces the drawn one; the marked spans' errors are added to it. The generated spans
  // that can error are the non-root ones (`errorSpan`), so the mark's count is clamped to them.
  const markedErrors = marked.filter((span) => span.error !== undefined).length;
  const generatedErrors = mark.error
    ? Math.min(mark.error.count, Math.max(0, trace.span_count - 1))
    : trace.error_count;
  const errorCount = generatedErrors + markedErrors;
  // Generated summaries always carry their tokens; the `?? 0` only narrows the optional fields.
  let inputTokens = trace.input_tokens ?? 0;
  let outputTokens = trace.output_tokens ?? 0;
  let costTotal = trace.cost_total;
  if (mark.costFactor !== undefined) {
    inputTokens = Math.floor(inputTokens * mark.costFactor);
    outputTokens = Math.floor(outputTokens * mark.costFactor);
    costTotal =
      Math.round(inputTokens * profile.price.input + outputTokens * profile.price.output) /
      1_000_000;
  }
  // The status-derived scores (`profile.scores`) follow the errored flag, so they are drawn again when a mark
  // changes it. The mark's own scores then replace theirs by name.
  const errored = errorCount > 0;
  const baseScores =
    errored !== trace.error_count > 0 ? profile.scores(index, errored) : trace.scores;
  return {
    ...trace,
    start_time: rfc3339(startMs),
    end_time: rfc3339(startMs + durationMs),
    duration_ns: durationMs * 1_000_000,
    status: errored ? "error" : "ok",
    name: mark.name ?? trace.name,
    release: mark.release ?? trace.release,
    tags: mark.tags ? [...new Set([...trace.tags, ...mark.tags])].sort() : trace.tags,
    span_count: trace.span_count + marked.length,
    error_count: errorCount,
    llm_count: trace.llm_count + marked.filter((span) => span.kind === "llm").length,
    input_tokens: inputTokens,
    output_tokens: outputTokens,
    total_tokens: inputTokens + outputTokens,
    cost_total: costTotal,
    scores: mark.scores ? markedScores(baseScores, index, mark.scores) : baseScores,
  };
}

/** The scores after a mark: one with a marked name is replaced (keeping its id), the others are added (M17). */
function markedScores(
  scores: readonly Schema<"ScoreBrief">[],
  index: number,
  changes: NonNullable<Mark["scores"]>,
): Schema<"ScoreBrief">[] {
  const result = [...scores];
  for (const change of changes) {
    const at = result.findIndex((score) => score.name === change.name);
    const current = result[at];
    if (current) {
      result[at] = { ...current, source: change.source, value: change.value };
    } else {
      result.push({
        name: change.name,
        score_id: markScoreUuid(index, result.length),
        source: change.source,
        value: change.value,
      });
    }
  }
  return result;
}

function traceSummary(
  seed: number,
  index: number,
  now: number,
  profile: Profile,
  call: CallIds | undefined,
): Schema<"TraceSummary"> {
  const showcase = showcaseAt(index, profile);
  if (showcase) {
    return showcaseSummary(showcase, index, now, profile);
  }
  const rng = mulberry32(mix(seed, index));
  const bigFirst = profile.bigFirstTrace && index === 0;
  const spanCount = bigFirst ? 10_000 : profile.spans.min + Math.floor(rng() * profile.spans.span);
  const errored = rng() < profile.errorRate;
  const errorCount = errored ? Math.min(spanCount, 1 + Math.floor(rng() * 3)) : 0;
  const environment = environmentOf(rng);
  const experiment = environment !== "cetra-eval" && rng() < 0.04;
  const ageMs = ageOf(rng, index, profile);
  const drawnMs = bigFirst
    ? 60_000
    : profile.duration.min + Math.floor(rng() ** profile.duration.skew * profile.duration.span);
  const startMs = now - ageMs;
  const name = profile.names[Math.floor(rng() * profile.names.length)] ?? profile.names[0];
  const model = profile.models[Math.floor(rng() * profile.models.length)] ?? profile.models[0];
  const inputTokens = profile.tokens.inMin + Math.floor(rng() * profile.tokens.inSpan);
  const outputTokens = profile.tokens.outMin + Math.floor(rng() * profile.tokens.outSpan);
  // Each roll is drawn for every profile, so the draws after it keep their place.
  const sessionRoll = rng();
  const session = sessionOf(profile.sessions, sessionRoll, rng, call);
  const userRoll = rng();
  const user = userOf(profile.users, userRoll, rng, call);
  const tags = pickTags(rng, profile);
  // A short-tagged trace is rescaled, with no new random draw. Trace 0 keeps its 60 s.
  const shortTag = profile.shortTag;
  const durationMs =
    shortTag && !bigFirst && tags.includes(shortTag.tag)
      ? shortTag.min +
        Math.floor(((drawnMs - profile.duration.min) * shortTag.span) / profile.duration.span)
      : drawnMs;
  const metadata: Record<string, string> = {};
  if (experiment) {
    metadata["cetra.experiment_id"] = `exp-${String(index % 12).padStart(2, "0")}`;
  }
  if (user) {
    Object.assign(metadata, userTraceMetadata(user, profile));
  }
  if (profile.traceMetadata) {
    Object.assign(metadata, profile.traceMetadata(index, ageMs));
  }
  const traceId = `a${index.toString(16).padStart(31, "0")}`;
  return {
    trace_id: traceId,
    root_span_id: spanId(index, 0),
    has_root: true,
    name,
    start_time: rfc3339(startMs),
    end_time: rfc3339(startMs + durationMs),
    duration_ns: durationMs * 1_000_000,
    status: errorCount > 0 ? "error" : "ok",
    environment,
    release: index % 2 === 0 ? "v1.5.0" : "v1.4.0",
    tags,
    metadata,
    span_count: spanCount,
    error_count: errorCount,
    llm_count: llmCount(spanCount, profile),
    input_tokens: inputTokens,
    output_tokens: outputTokens,
    total_tokens: inputTokens + outputTokens,
    cost_total:
      Math.round(inputTokens * profile.price.input + outputTokens * profile.price.output) /
      1_000_000,
    models: [model],
    scores: profile.scores(index, errorCount > 0),
    ...(session ? { session_id: session } : {}),
    ...(user ? { user_id: user } : {}),
  };
}

/** A showcase trace's summary (M11, M12). Its tokens and cost are the sums over `showcase.spans`. */
function showcaseSummary(
  showcase: Showcase,
  index: number,
  now: number,
  profile: Profile,
): Schema<"TraceSummary"> {
  const startMs = now - showcase.ageMs;
  const durationMs = showcase.spans[0].to;
  const inputTokens = showcase.spans.reduce((sum, row) => sum + (row.inputTokens ?? 0), 0);
  const outputTokens = showcase.spans.reduce((sum, row) => sum + (row.outputTokens ?? 0), 0);
  const cost = showcase.spans.reduce((sum, row) => sum + (row.cost ?? 0), 0);
  const errorCount = showcase.spans.filter((row) => row.status === "error").length;
  return {
    trace_id: `a${index.toString(16).padStart(31, "0")}`,
    root_span_id: spanId(index, 0),
    has_root: true,
    name: showcase.spans[0].name,
    start_time: rfc3339(startMs),
    end_time: rfc3339(startMs + durationMs),
    duration_ns: durationMs * 1_000_000,
    status: errorCount > 0 ? "error" : "ok",
    environment: "production",
    release: "v1.5.0",
    tags: showcase.tags,
    metadata: showcase.user ? userTraceMetadata(showcase.user, profile) : {},
    span_count: showcase.spans.length,
    error_count: errorCount,
    llm_count: showcase.spans.filter((row) => row.kind === "llm").length,
    input_tokens: inputTokens,
    output_tokens: outputTokens,
    total_tokens: inputTokens + outputTokens,
    cost_total: Math.round(cost * 1_000_000) / 1_000_000,
    // The generation models only: the embedding and rerank models appear on their spans.
    models: showcase.models,
    scores: showcase.scores.map((score, i) => ({
      name: score.name,
      score_id: scoreUuid(index, i),
      source: score.source,
      value: score.value,
    })),
    ...(showcase.user ? { user_id: showcase.user } : {}),
    ...(showcase.session ? { session_id: showcase.session } : {}),
  };
}

/**
 * `user.*` metadata of a user's traces, derived from the id's trailing digits (`user-NNN`) so it is stable. The name
 * is the profile's label and those digits. Values are JSON-encoded (spec 01).
 */
function userTraceMetadata(userId: string, profile: Profile): Record<string, string> {
  const digits = /(\d+)$/.exec(userId)?.[1] ?? "";
  const index = Number.parseInt(digits || "0", 10);
  const label = profile.users.kind === "none" ? "User" : profile.users.label;
  return {
    "user.name": JSON.stringify(`${label} ${digits}`),
    "user.plan": JSON.stringify(USER_PLANS[index % USER_PLANS.length] ?? USER_PLANS[0]),
    "user.country": JSON.stringify(
      USER_COUNTRIES[index % USER_COUNTRIES.length] ?? USER_COUNTRIES[0],
    ),
  };
}

function buildDetail(
  seed: number,
  index: number,
  trace: Schema<"TraceSummary">,
  profile: Profile,
  mark: Mark | undefined,
): Schema<"TraceDetail"> {
  const showcase = showcaseAt(index, profile);
  if (showcase) {
    return showcaseDetail(showcase, index, trace);
  }
  const rng = mulberry32(mix(seed, index + 17));
  // The generated spans come first and keep their draws; a mark's spans follow them under the root (M17).
  const markedSpans = mark?.spans ?? [];
  const spanCount = trace.span_count - markedSpans.length;
  const markedErrors = markedSpans.filter((span) => span.error !== undefined).length;
  const bigFirst = profile.bigFirstTrace && index === 0;
  const maxDepth = bigFirst ? 8 : profile.spans.maxDepth;
  const shape = spanShape(spanCount, maxDepth, bigFirst ? 2_000 : 0, rng);
  const startMs = Date.parse(trace.start_time);
  const endMs = Date.parse(trace.end_time);
  const built = layoutSpans(shape, startMs, endMs, trace.error_count - markedErrors, rng, profile);
  const spans = built.map((span, spanIndex) =>
    treeSpan(span, spanIndex, index, trace, profile, mark),
  );
  const marked = markedSpans.map((span, i) =>
    markedTreeSpan(span, spanCount + i, index, trace, profile),
  );
  return { trace, spans: [...spans, ...marked], truncated: false };
}

/** A mark's span under the root, at `position` after the generated spans (M17). */
function markedTreeSpan(
  span: MarkedSpan,
  position: number,
  traceIndex: number,
  trace: Schema<"TraceSummary">,
  profile: Profile,
): Schema<"TreeSpan"> {
  const startMs = Date.parse(trace.start_time) + span.fromMs;
  const row = treeSpan(
    {
      depth: 1,
      detached: false,
      parent: 0,
      startMs,
      endMs: startMs + span.durationMs,
      kind: span.kind,
      status: span.error ? "error" : "ok",
    },
    position,
    traceIndex,
    trace,
    profile,
  );
  return {
    ...row,
    span: {
      ...row.span,
      name: span.name,
      ...(span.error ? { status_message: span.error.message } : {}),
    },
  };
}

/** A showcase trace's tree: its spans come in `showcase.spans` order, so every parent comes before its children (M11, M12). */
function showcaseDetail(
  showcase: Showcase,
  index: number,
  trace: Schema<"TraceSummary">,
): Schema<"TraceDetail"> {
  const startMs = Date.parse(trace.start_time);
  const depths: number[] = [];
  const spans = showcase.spans.map((row, position): Schema<"TreeSpan"> => {
    const depth = row.parent === undefined ? 0 : (depths[row.parent] ?? 0) + 1;
    depths[position] = depth;
    const summary: Schema<"SpanSummary"> = {
      span_id: spanId(index, position),
      trace_id: trace.trace_id,
      span_kind: row.kind,
      name: row.name,
      start_time: rfc3339(startMs + row.from),
      end_time: rfc3339(startMs + row.to),
      duration_ns: (row.to - row.from) * 1_000_000,
      status: row.status ?? "ok",
      ...(row.statusMessage ? { status_message: row.statusMessage } : {}),
      metadata: { ...trace.metadata },
      tags: trace.tags,
      ...(row.parent === undefined ? {} : { parent_span_id: spanId(index, row.parent) }),
      ...(trace.environment ? { environment: trace.environment } : {}),
      service_name: showcase.service,
      ...(trace.user_id ? { user_id: trace.user_id } : {}),
      ...(trace.release ? { release: trace.release } : {}),
      ...(row.model ? { model: row.model } : {}),
      ...(row.provider ? { provider: row.provider } : {}),
      ...(row.inputTokens === undefined || row.outputTokens === undefined
        ? {}
        : {
            input_tokens: row.inputTokens,
            output_tokens: row.outputTokens,
            total_tokens: row.inputTokens + row.outputTokens,
          }),
      ...(row.cost === undefined ? {} : { cost_total: row.cost }),
      ...(row.ttftMs === undefined ? {} : { ttft_ns: row.ttftMs * 1_000_000 }),
      ...(row.prompt ? { prompt_name: row.prompt.name, prompt_version: row.prompt.version } : {}),
    };
    if (position === 0) {
      // The root carries the trace's totals and previews, as `treeSpan` gives every root.
      summary.input_preview = trace.input_preview;
      summary.output_preview = trace.output_preview;
      summary.input_tokens = trace.input_tokens;
      summary.output_tokens = trace.output_tokens;
      summary.total_tokens = trace.total_tokens;
      summary.cost_total = trace.cost_total;
    }
    return { depth, detached: false, span: summary };
  });
  return { trace, spans, truncated: false };
}

function spanShape(
  spanCount: number,
  maxDepth: number,
  detachedEvery: number,
  rng: () => number,
): { depth: number; detached: boolean }[] {
  const rows = [{ depth: 0, detached: false }];
  for (let index = 1; index < spanCount; index += 1) {
    if (detachedEvery > 0 && index % detachedEvery === 0) {
      rows.push({ depth: 0, detached: true });
      continue;
    }
    const previous = rows[index - 1]?.depth ?? 0;
    const ceiling = Math.min(maxDepth, previous + 1);
    const depth = rng() < 0.65 ? ceiling : 1 + Math.floor(rng() * Math.max(1, ceiling - 1));
    rows.push({ depth: Math.min(ceiling, Math.max(1, depth)), detached: false });
  }
  return rows;
}

function layoutSpans(
  shape: { depth: number; detached: boolean }[],
  startMs: number,
  endMs: number,
  errorCount: number,
  rng: () => number,
  profile: Profile,
): BuiltSpan[] {
  const built: BuiltSpan[] = [];
  const stack: number[] = [];
  const lastChild = new Map<number, number>();
  for (let index = 0; index < shape.length; index += 1) {
    const row = shape[index];
    if (!row) {
      continue;
    }
    const kind = kindAt(index, profile);
    const status = errorSpan(index, shape.length, errorCount) ? "error" : "ok";
    if (row.detached || row.depth === 0) {
      stack.length = 0;
      stack[0] = index;
      built.push({
        depth: 0,
        detached: row.detached,
        startMs,
        endMs,
        kind,
        status,
      });
      continue;
    }
    const parentIndex = stack[row.depth - 1] ?? 0;
    const parent = built[parentIndex];
    const previousIndex = lastChild.get(parentIndex);
    const previous = previousIndex === undefined ? undefined : built[previousIndex];
    const window = childWindow(parent ?? { startMs, endMs }, previous, kind, rng);
    stack.length = row.depth;
    stack[row.depth] = index;
    lastChild.set(parentIndex, index);
    built.push({
      depth: row.depth,
      detached: false,
      parent: parentIndex,
      startMs: window.startMs,
      endMs: window.endMs,
      kind,
      status,
    });
  }
  return built;
}

function childWindow(
  parent: { startMs: number; endMs: number },
  previous: BuiltSpan | undefined,
  kind: Schema<"SpanKind">,
  rng: () => number,
): { startMs: number; endMs: number } {
  const span = Math.max(1, parent.endMs - parent.startMs);
  let startMs = parent.startMs + Math.floor(rng() * span);
  let length = Math.max(1, Math.floor(rng() * Math.max(1, parent.endMs - startMs)));
  if (previous?.kind === "llm" && kind === "llm") {
    const previousSpan = Math.max(1, previous.endMs - previous.startMs);
    startMs = previous.startMs + Math.floor(previousSpan * 0.4);
    length = Math.max(1, Math.floor(previousSpan * 0.8));
  }
  if (startMs < parent.startMs || startMs >= parent.endMs) {
    startMs = parent.startMs;
  }
  let endMs = Math.min(parent.endMs, startMs + length);
  if (endMs <= startMs) {
    endMs = Math.min(parent.endMs, startMs + 1);
  }
  if (endMs <= startMs) {
    return { startMs: parent.startMs, endMs: parent.endMs };
  }
  return { startMs, endMs };
}

function treeSpan(
  span: BuiltSpan,
  spanIndex: number,
  traceIndex: number,
  trace: Schema<"TraceSummary">,
  profile: Profile,
  mark?: Mark,
): Schema<"TreeSpan"> {
  const id = spanId(traceIndex, spanIndex);
  const names = profile.spanNames[span.kind];
  // A generated error span takes the mark's error, when the mark has one (M17).
  const markedError = span.status === "error" && spanIndex > 0 ? mark?.error : undefined;
  const summary: Schema<"SpanSummary"> = {
    span_id: id,
    trace_id: trace.trace_id,
    span_kind: span.kind,
    name:
      spanIndex === 0
        ? trace.name
        : markedError
          ? markedError.spanName
          : names
            ? pick(names, spanIndex)
            : `${span.kind} ${spanIndex}`,
    start_time: rfc3339(span.startMs),
    end_time: rfc3339(span.endMs),
    duration_ns: (span.endMs - span.startMs) * 1_000_000,
    status: span.status,
    metadata: { ...trace.metadata },
    tags: trace.tags,
    ...(span.status === "error"
      ? { status_message: markedError?.message ?? profile.error.message }
      : {}),
    ...(span.parent === undefined ? {} : { parent_span_id: spanId(traceIndex, span.parent) }),
    ...(trace.environment ? { environment: trace.environment } : {}),
    service_name: profile.service,
    ...(trace.session_id ? { session_id: trace.session_id } : {}),
    ...(trace.user_id ? { user_id: trace.user_id } : {}),
    ...(trace.release ? { release: trace.release } : {}),
    ...(span.kind === "llm" && trace.models[0]
      ? { model: trace.models[0], provider: profile.provider }
      : {}),
  };
  if (spanIndex === 0) {
    summary.input_preview = trace.input_preview;
    summary.output_preview = trace.output_preview;
    summary.input_tokens = trace.input_tokens;
    summary.output_tokens = trace.output_tokens;
    summary.total_tokens = trace.total_tokens;
    summary.cost_total = trace.cost_total;
  }
  return { depth: span.depth, detached: span.detached, span: summary };
}

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

/** The question a trace answers: turn 1 of a session is the topic's question, later turns its follow-ups. */
type TraceTurn = { topic: Turn; exchange: Exchange; main: boolean };

type SpanContext = {
  traceIndex: number;
  errored: boolean;
  turn: TraceTurn;
  profile: Profile;
  /** The trace's mark (M17). Its spans from `generatedCount` on are the mark's, in order. */
  mark?: Mark;
  generatedCount: number;
};

const FENCE = "```";
const FAILED_ANSWER =
  "I could not finish.\n\n- The lookup tool failed\n- Escalate to a human agent";
const TOOL_NAMES = ["lookup_ticket", "get_customer", "search_orders"] as const;
const RETRIEVER_SCORES = [0.92, 0.81, 0.67] as const;
const RERANK_SCORES = [0.52, 0.91, 0.84] as const;

export const SUPPORT_TURNS: readonly [Turn, ...Turn[]] = [
  {
    question: "How do I get a refund for a duplicate charge?",
    heading: "Refunds for duplicate charges",
    summary: "A duplicate charge can be refunded from the billing page within 14 days.",
    steps: [
      "Open Billing and select the duplicate invoice",
      "Choose Refund and confirm the amount",
      "The money returns in 3 to 5 business days",
    ],
    snippets: {
      python:
        'refund = client.billing.refund(invoice_id="inv_1042", reason="duplicate")\nprint(refund.status)',
      ts: 'const refund = await client.billing.refund({ invoiceId: "inv_1042", reason: "duplicate" });\nconsole.log(refund.status);',
    },
    docs: [
      "Refunds are issued to the original payment method.",
      "Duplicate charges are reversed within 14 days.",
      "Partial refunds are supported on invoices.",
    ],
    followUps: [
      {
        question: "Does the refund go back to the same card?",
        summary: "Yes. A refund returns to the payment method that paid the invoice.",
        steps: [
          "Check the payment method on the invoice",
          "Refunds never go to a different card",
          "Ask support if the original card has expired",
        ],
      },
      {
        question: "Can I refund only one line of a large invoice?",
        summary: "Yes. Partial refunds are supported on any invoice.",
        steps: [
          "Open the invoice and choose Partial refund",
          "Enter the amount to return",
          "Check the refund status once you confirm",
        ],
      },
    ],
  },
  {
    question: "I forgot my password. How do I reset it?",
    heading: "Resetting a password",
    summary: "Use the reset link on the sign-in page. The link expires after 30 minutes.",
    steps: [
      "Click Forgot password on the sign-in page",
      "Open the email link within 30 minutes",
      "Choose a new password of at least 12 characters",
    ],
    snippets: {
      python: 'client.auth.request_password_reset(email="user@example.com")',
      ts: 'await client.auth.requestPasswordReset({ email: "user@example.com" });',
    },
    docs: [
      "Password reset links expire after 30 minutes.",
      "Reset emails come from the no-reply address.",
      "Accounts with SSO reset their password at the identity provider.",
    ],
    followUps: [
      {
        question: "The reset email never arrived. What should I do?",
        summary: "Check your spam folder first, then ask for a new link.",
        steps: [
          "Look in the spam or junk folder",
          "Make sure you typed the right email address",
          "Request a new link from the sign-in page",
        ],
      },
      {
        question: "Can I change my password without the email?",
        summary: "Yes. Signed-in users can change it from their profile.",
        steps: [
          "Open Profile and then Security",
          "Choose Change password",
          "Enter your current password and the new one",
        ],
      },
    ],
  },
  {
    question: "Why am I getting 429 errors from the API?",
    heading: "API rate limits",
    summary: "A 429 response means the project passed its per-minute request limit.",
    steps: [
      "Read the Retry-After header",
      "Back off with an exponential delay",
      "Ask for a higher limit if traffic is steady",
    ],
    snippets: {
      python: 'time.sleep(float(response.headers["Retry-After"]))',
      ts: 'await sleep(Number(response.headers.get("Retry-After")) * 1000);',
    },
    docs: [
      "The default limit is 600 requests per minute per project.",
      "Retry-After gives the number of seconds to wait.",
      "Limits are shared by every key in a project.",
    ],
    followUps: [
      {
        question: "How can I see how many requests I have sent?",
        summary: "The Usage page shows requests per minute for each project.",
        steps: [
          "Open Settings and then Usage",
          "Select the project and the time range",
          "Compare the peak with the per-minute limit",
        ],
      },
      {
        question: "Do all my API keys share the same limit?",
        summary: "Yes. The limit applies to the whole project, not to each key.",
        steps: [
          "Count the requests from every key in the project",
          "Move heavy batch jobs to a separate project",
          "Spread batch jobs out to smooth the peaks",
        ],
      },
    ],
  },
  {
    question: "How can I export my traces to a CSV file?",
    heading: "Exporting traces",
    summary: "Export from the Traces page, or call the export endpoint for large ranges.",
    steps: [
      "Filter the traces you need",
      "Choose Export and then CSV",
      "Use the API for ranges over 100,000 traces",
    ],
    snippets: {
      python:
        'export = client.traces.export(format="csv", since="2026-11-01")\nexport.save("traces.csv")',
      ts: 'const file = await client.traces.export({ format: "csv", since: "2026-11-01" });\nawait file.save("traces.csv");',
    },
    docs: [
      "Exports include every filtered column.",
      "CSV exports are limited to 100,000 rows.",
      "Exports are kept for 7 days.",
    ],
    followUps: [
      {
        question: "How long can I download an export?",
        summary: "Exports are kept for 7 days, then they are removed.",
        steps: [
          "Download the file within 7 days",
          "Request a new export after that",
          "Use the API to script regular exports",
        ],
      },
      {
        question: "What if my export has more rows than the CSV limit?",
        summary: "Split the date range into smaller parts and export each one.",
        steps: [
          "Pick a shorter date range",
          "Export each range separately",
          "Combine the files in your data tool",
        ],
      },
    ],
  },
  {
    question: "How do I set up a webhook for new tickets?",
    heading: "Webhook setup",
    summary: "Register an HTTPS endpoint and subscribe it to the ticket.created event.",
    steps: [
      "Open Settings and then Webhooks",
      "Add an HTTPS URL",
      "Verify the signature header on each request",
    ],
    snippets: {
      python:
        'hook = client.webhooks.create(url="https://example.com/hooks", events=["ticket.created"])',
      ts: 'const hook = await client.webhooks.create({ url: "https://example.com/hooks", events: ["ticket.created"] });',
    },
    docs: [
      "Webhooks retry failed deliveries three times.",
      "Each request carries an HMAC signature header.",
      "Only HTTPS endpoints are accepted.",
    ],
    followUps: [
      {
        question: "Why are my webhook requests failing?",
        summary: "Check that the endpoint is public and returns a success status.",
        steps: [
          "Open the delivery log for the webhook",
          "Confirm the URL uses HTTPS",
          "Return a success status code from the endpoint",
        ],
      },
      {
        question: "How do I test a webhook before going live?",
        summary: "Send a test event from the webhook page to your endpoint.",
        steps: [
          "Open the webhook and choose Send test event",
          "Check the signature header in your handler",
          "Read the delivery log for the response",
        ],
      },
    ],
  },
  {
    question: "Why was my plan price changed this month?",
    heading: "Plan price changes",
    summary: "Prices change at the start of a billing cycle, not in the middle.",
    steps: [
      "Check the plan history in Billing",
      "Changes apply on the next invoice",
      "Contact support to request a credit",
    ],
    snippets: {
      python: "plan = client.billing.current_plan()\nprint(plan.name, plan.price_cents)",
      ts: "const plan = await client.billing.currentPlan();\nconsole.log(plan.name, plan.priceCents);",
    },
    docs: [
      "Plan changes take effect on the next billing date.",
      "Invoices list the price used for each cycle.",
      "Credits appear on the following invoice.",
    ],
    followUps: [
      {
        question: "Where can I see my past invoices?",
        summary: "Every invoice is listed under Billing, with the price used for each cycle.",
        steps: [
          "Open Billing and then Invoices",
          "Select the invoice for the month you need",
          "Keep a copy for your records",
        ],
      },
      {
        question: "Can I switch back to my old plan?",
        summary: "Yes. A plan change applies from the next billing date.",
        steps: [
          "Open Billing and choose Change plan",
          "Select the previous plan",
          "The switch takes effect on the next invoice",
        ],
      },
    ],
  },
];

/** Picks an item cycling by index; the list is never empty, so the first item is the fallback. */
function pick<T>(items: readonly [T, ...T[]], index: number): T {
  const [first] = items;
  return items[index % items.length] ?? first;
}

const TURN_GAP_MIN_MS = 30_000;
const TURN_GAP_MAX_MS = 480_000;
const TURN_GAP_SALT = 0x6a09e667;
/** The youngest trace is a minute old (see `ageOf`); a session's last turn never starts later than that. */
const MIN_TRACE_AGE_MS = 60_000;

/** The support bot's generation (M11 to M15): the draws and tables every support dataset has always had. */
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
  error: {
    message: "tool failed",
    stacktrace: "Error: tool failed\n    at lookup (agent.ts:12:5)",
  },
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
  showcases: [SUPPORT_SHOWCASE],
};

/**
 * Spreads each session's turns in time (SL-6): the session keeps its earliest trace's start, and each later
 * turn (in start-time order; in index order for `calls`) starts 30 s to 8 min after the previous one. A session whose last turn would
 * start after `now` moves earlier as a whole. Traces keep their own durations.
 */
function spreadSessionTurns(
  traces: readonly Schema<"TraceSummary">[],
  seed: number,
  now: number,
  profile: Profile,
): Schema<"TraceSummary">[] {
  const groups = new Map<string, number[]>();
  traces.forEach((trace, index) => {
    if (trace.session_id) {
      groups.set(trace.session_id, [...(groups.get(trace.session_id) ?? []), index]);
    }
  });
  const starts = traces.map((trace) => Date.parse(trace.start_time));
  const timed = [...traces];
  for (const indices of groups.values()) {
    // Start-time order, ties by index: the same order `sessionSlots` numbers the turns in. A call's
    // turns are generated in order, so it runs in index order, anchored at its first trace's start.
    const ordered =
      profile.sessions.kind === "calls"
        ? [...indices].sort((left, right) => left - right)
        : [...indices].sort(
            (left, right) => (starts[left] ?? 0) - (starts[right] ?? 0) || left - right,
          );
    const [first, ...later] = ordered;
    if (first === undefined) {
      continue;
    }
    const chain = new Map<number, number>([[first, starts[first] ?? 0]]);
    let previous = starts[first] ?? 0;
    for (const index of later) {
      const random = mulberry32(mix(seed ^ TURN_GAP_SALT, index));
      const { min, max } = profile.turnGap;
      previous += min + Math.floor(random() * (max - min + 1));
      chain.set(index, previous);
    }
    const shift = Math.max(0, previous - (now - MIN_TRACE_AGE_MS));
    for (const [index, start] of chain) {
      const trace = traces[index];
      if (!trace) {
        continue;
      }
      const moved = start - shift;
      timed[index] = {
        ...trace,
        start_time: rfc3339(moved),
        end_time: rfc3339(moved + trace.duration_ns / 1_000_000),
      };
    }
  }
  return timed;
}

/** Each trace's place in its session: the topic index (the session's first trace's index) and its turn from 1. */
function sessionSlots(
  traces: readonly Schema<"TraceSummary">[],
): Map<number, { topic: number; turn: number }> {
  const groups = new Map<string, number[]>();
  traces.forEach((trace, index) => {
    if (trace.session_id) {
      const rows = groups.get(trace.session_id) ?? [];
      rows.push(index);
      groups.set(trace.session_id, rows);
    }
  });
  const slots = new Map<number, { topic: number; turn: number }>();
  for (const indices of groups.values()) {
    const topic = indices[0] ?? 0;
    // Start-time order, ties by index: the same order the session page lists its traces in.
    const ordered = [...indices].sort((left, right) => {
      const byStart = (traces[left]?.start_time ?? "").localeCompare(
        traces[right]?.start_time ?? "",
      );
      return byStart !== 0 ? byStart : left - right;
    });
    for (const [position, index] of ordered.entries()) {
      slots.set(index, { topic, turn: position + 1 });
    }
  }
  return slots;
}

/** Turn 1 is the topic's own question; turn k (k >= 2) is its follow-up `(k - 2) mod count`. */
function turnOf(topicIndex: number, turn: number, profile: Profile): TraceTurn {
  const topic = pick(profile.turns, topicIndex);
  if (turn <= 1) {
    return { topic, exchange: topic, main: true };
  }
  return { topic, exchange: pick(topic.followUps, turn - 2), main: false };
}

function withPreviews(
  trace: Schema<"TraceSummary">,
  turn: TraceTurn,
  profile: Profile,
): Schema<"TraceSummary"> {
  return {
    ...trace,
    input_preview: turn.exchange.question,
    output_preview: firstLine(
      trace.error_count > 0 ? profile.failedAnswer : shortAnswer(turn.exchange),
    ),
  };
}

function firstLine(text: string): string {
  return text.split("\n")[0] ?? "";
}

function shortAnswer(turn: Exchange): string {
  return [turn.summary, "", ...turn.steps.map((step) => `- ${step}`)].join("\n");
}

/** The main answer: its heading, summary, steps and code block. A turn without snippets answers as a follow-up does. */
function llmAnswer(turn: Turn, language: "python" | "ts"): string {
  const snippet = turn.snippets?.[language];
  if (snippet === undefined) {
    return shortAnswer(turn);
  }
  return [
    `## ${turn.heading}`,
    "",
    turn.summary,
    "",
    ...turn.steps.map((step) => `- ${step}`),
    "",
    `${FENCE}${language}`,
    snippet,
    FENCE,
  ].join("\n");
}

function scoredDocs(
  turn: Turn,
  scores: readonly [number, number, number],
): { id: string; score: number; text: string }[] {
  return (turn.docs ?? []).map((text, i) => ({ id: `doc-${i + 1}`, score: scores[i] ?? 0, text }));
}

function spanFromSummary(span: Schema<"SpanSummary">, context: SpanContext): Schema<"SpanDetail"> {
  const index = spanIndex(span.span_id);
  // Every span of a trace shares the trace's turn, the same one its preview and root use.
  const turn = context.turn.topic;
  const exchange = context.turn.exchange;
  const truncated = spanIndex(span.span_id) % 17 === 0 ? ["input"] : [];
  const detail: Schema<"SpanDetail"> = {
    ...span,
    attributes: { "gen_ai.operation.name": span.span_kind },
    conventions: ["genai"],
    events:
      span.status === "error"
        ? [
            {
              name: "exception",
              time: span.end_time,
              attributes: {
                "exception.stacktrace": stackTraceOf(context, index),
              },
            },
          ]
        : [],
    ingest_time: span.end_time,
    links: [],
    media_refs: [],
    resource_attributes: { "host.name": "mock" },
    schema_version: 1,
    truncated,
    scope_name: "cetra",
  };
  const marked =
    index >= context.generatedCount
      ? context.mark?.spans?.[index - context.generatedCount]
      : undefined;
  if (marked) {
    // A marked span's payloads are the mark's; an absent one stays absent.
    if (marked.input) {
      detail.input = marked.input;
    }
    if (marked.output) {
      detail.output = marked.output;
    }
  } else if (span.span_kind === "llm") {
    const language = index % 2 === 0 ? "python" : "ts";
    detail.input = {
      format: "messages",
      value: [
        {
          role: "system",
          parts: [
            {
              type: "text",
              content: context.profile.systemPrompt,
            },
          ],
        },
        { role: "user", parts: [{ type: "text", content: exchange.question }] },
      ],
    };
    detail.output = {
      format: "messages",
      value: [
        {
          role: "assistant",
          parts: [
            {
              type: "text",
              // Only the main answer carries a code block; follow-ups are a paragraph and a list.
              content: context.turn.main ? llmAnswer(turn, language) : shortAnswer(exchange),
            },
          ],
        },
      ],
    };
  } else if (span.span_kind === "tool") {
    const tool = context.profile.tool(context.traceIndex);
    const name = pick(context.profile.spanNames.tool ?? TOOL_NAMES, index);
    detail.input = { format: "json", value: { name, arguments: tool.arguments } };
    detail.output = { format: "json", value: tool.result };
  } else if (span.span_kind === "retriever") {
    detail.input = { format: "text", value: exchange.question };
    detail.output = {
      format: "json",
      value: { documents: scoredDocs(turn, RETRIEVER_SCORES) },
    };
  } else if (span.span_kind === "embedding") {
    detail.input = { format: "text", value: exchange.question };
    detail.output = {
      format: "json",
      value: { dimensions: 1536, model: "text-embedding-3-small" },
    };
  } else if (span.span_kind === "guardrail") {
    const passed = index % 5 !== 4;
    detail.input = { format: "text", value: exchange.question };
    detail.output = {
      format: "json",
      value: {
        passed,
        reasons: passed
          ? []
          : [`The answer does not follow the policy for ${turn.heading.toLowerCase()}.`],
      },
    };
  } else if (span.span_kind === "evaluator") {
    const score = (index % 10) / 10;
    detail.input = { format: "text", value: exchange.question };
    detail.output = {
      format: "json",
      value: {
        score,
        reasoning:
          score >= 0.5
            ? "The answer cites the policy and stays on topic."
            : "The answer leaves out a required step.",
      },
    };
  } else if (span.span_kind === "reranker") {
    detail.input = {
      format: "json",
      value: {
        query: exchange.question,
        documents: (turn.docs ?? []).map((_doc, i) => `doc-${i + 1}`),
      },
    };
    detail.output = {
      format: "json",
      value: {
        documents: scoredDocs(turn, RERANK_SCORES).sort((left, right) => right.score - left.score),
      },
    };
  } else if (span.span_kind === "span") {
    detail.input = { format: "text", value: exchange.question };
    detail.output = { format: "text", value: `Answered with **${turn.heading}**.` };
  } else {
    // agent (the root span) and chain: the user's question and a short Markdown answer.
    const failed = index === 0 && context.errored;
    detail.input = { format: "text", value: exchange.question };
    detail.output = {
      format: "text",
      value: failed ? context.profile.failedAnswer : shortAnswer(exchange),
    };
  }
  const showcase = showcaseAt(context.traceIndex, context.profile);
  if (showcase) {
    // The generic rule flags every 17th span's input, the root included; the hero shows no truncation badge.
    detail.truncated = [];
    if (showcase.story) {
      applyStoryPayload(detail, showcase.story.payloads[spanIndex(detail.span_id)]);
    } else {
      applySupportPayload(detail, context.traceIndex, context.turn.exchange.question);
    }
  }
  return detail;
}

/**
 * The stack trace of an error span at `position`: a marked span's own error, a generated error span's mark error,
 * or the profile's stack trace (M17).
 */
function stackTraceOf(context: SpanContext, position: number): string {
  if (position >= context.generatedCount) {
    const marked = context.mark?.spans?.[position - context.generatedCount];
    return marked?.error?.stacktrace ?? context.profile.error.stacktrace;
  }
  return context.mark?.error?.stacktrace ?? context.profile.error.stacktrace;
}

/** A story span's hand-written payloads; `error` replaces the generic exception event of an error span. */
function applyStoryPayload(
  detail: Schema<"SpanDetail">,
  payload: ShowcasePayload | undefined,
): void {
  if (!payload) {
    return;
  }
  detail.input = payload.input;
  detail.output = payload.output;
  if (payload.error) {
    detail.events = [
      {
        name: "exception",
        time: detail.end_time,
        attributes: {
          "exception.type": payload.error.type,
          "exception.message": payload.error.message,
          "exception.stacktrace": payload.error.stacktrace,
        },
      },
    ];
  }
}

/**
 * The support trace's hand-written payloads (M11). Spans not listed here keep the generic payload of their kind.
 * `traceIndex` is the trace's index, so the span ids are the trace's own.
 */
function applySupportPayload(
  detail: Schema<"SpanDetail">,
  traceIndex: number,
  question: string,
): void {
  const at = (position: number) => spanId(traceIndex, position);
  switch (detail.span_id) {
    case at(1):
      detail.input = { format: "text", value: question };
      detail.output = { format: "json", value: { passed: true, reasons: [] } };
      break;
    case at(2):
      detail.input = {
        format: "messages",
        value: [
          {
            role: "system",
            parts: [
              {
                type: "text",
                content:
                  "Classify the user's request into one intent: billing, account, export_data, api_errors, other. Reply with JSON.",
              },
            ],
          },
          { role: "user", parts: [{ type: "text", content: question }] },
        ],
      };
      detail.output = {
        format: "messages",
        value: [
          {
            role: "assistant",
            parts: [
              {
                type: "text",
                content: '{"intent": "export_data", "confidence": 0.97}',
              },
            ],
          },
        ],
      };
      break;
    case at(7):
      detail.input = {
        format: "messages",
        value: [
          {
            role: "system",
            parts: [
              {
                type: "text",
                content: "You are a support agent. Plan the tool calls you need before answering.",
              },
            ],
          },
          { role: "user", parts: [{ type: "text", content: question }] },
        ],
      };
      detail.output = {
        format: "messages",
        value: [
          {
            role: "assistant",
            parts: [
              {
                type: "text",
                content: [
                  "1. get_account: check the plan's export limits.",
                  "2. check_export_limits: rows and formats allowed.",
                  "3. Answer with the Traces page steps and the API for large ranges.",
                ].join("\n"),
              },
            ],
          },
        ],
      };
      break;
    case at(8):
      detail.input = {
        format: "json",
        value: { name: "get_account", arguments: { user_id: "user-012" } },
      };
      detail.output = { format: "json", value: { plan: "pro", seats: 8, region: "eu1" } };
      break;
    case at(9):
      detail.input = {
        format: "json",
        value: { name: "check_export_limits", arguments: { plan: "pro", format: "csv" } },
      };
      detail.output = {
        format: "json",
        value: { max_rows: 100_000, formats: ["csv", "ndjson"], retention_days: 7 },
      };
      break;
    case at(11):
      detail.input = {
        format: "json",
        value: {
          text: "Export from the Traces page, or call the export endpoint for large ranges.",
        },
      };
      detail.output = { format: "json", value: { passed: true, redacted: 0 } };
      break;
    case at(12):
      detail.input = { format: "text", value: question };
      detail.output = {
        format: "json",
        value: {
          score: 0.9,
          reasoning: "The answer gives the steps and the API route for large exports.",
        },
      };
      break;
    default:
      break;
  }
}

function sessionsFrom(traces: readonly Schema<"TraceSummary">[]): Schema<"SessionSummary">[] {
  const groups = new Map<string, Schema<"TraceSummary">[]>();
  for (const trace of traces) {
    if (!trace.session_id) {
      continue;
    }
    const rows = groups.get(trace.session_id) ?? [];
    rows.push(trace);
    groups.set(trace.session_id, rows);
  }
  return [...groups.entries()].map(([sessionId, rows]) => {
    // Start-time order, ties by trace id (SL-1): the first and last turns are the earliest and latest.
    const ordered = [...rows].sort(
      (left, right) =>
        left.start_time.localeCompare(right.start_time) ||
        left.trace_id.localeCompare(right.trace_id),
    );
    const start = Date.parse(ordered[0]?.start_time ?? "");
    const end = ordered.reduce(
      (latest, trace) => Math.max(latest, Date.parse(trace.end_time)),
      start,
    );
    const scores = new Map<string, Schema<"ScoreBrief">>();
    const tags = new Set<string>();
    let errors = 0;
    let errorTraces = 0;
    let cost = 0;
    let tokens = 0;
    let userId: string | undefined;
    for (const trace of ordered) {
      errors += trace.error_count;
      if (trace.status === "error") {
        errorTraces += 1;
      }
      cost += trace.cost_total ?? 0;
      tokens += trace.total_tokens ?? 0;
      userId = trace.user_id ?? userId;
      for (const tag of trace.tags) {
        tags.add(tag);
      }
      for (const score of trace.scores) {
        scores.set(score.name, score);
      }
    }
    const firstInput = ordered[0]?.input_preview;
    const lastOutput = ordered[ordered.length - 1]?.output_preview;
    return {
      session_id: sessionId,
      start_time: rfc3339(start),
      end_time: rfc3339(end),
      duration_ns: Math.max(0, end - start) * 1_000_000,
      error_count: errors,
      error_trace_count: errorTraces,
      ...(firstInput === undefined ? {} : { first_input_preview: firstInput }),
      ...(lastOutput === undefined ? {} : { last_output_preview: lastOutput }),
      trace_count: ordered.length,
      cost_total: cost,
      total_tokens: tokens,
      tags: [...tags].sort(),
      scores: [...scores.values()],
      ...(userId ? { user_id: userId } : {}),
    };
  });
}

/** A user summary without `activity`, which depends on the response's window (see `userSummaries`). */
export type UserGroup = {
  user: Omit<Schema<"EndUserSummary">, "activity">;
  traces: Schema<"TraceSummary">[];
};

const ACTIVITY_BUCKETS = 24;
const USER_METADATA_LIMIT = 20;

function usersFrom(traces: readonly Schema<"TraceSummary">[]): UserGroup[] {
  const groups = new Map<string, Schema<"TraceSummary">[]>();
  for (const trace of traces) {
    if (!trace.user_id) {
      continue;
    }
    const rows = groups.get(trace.user_id) ?? [];
    rows.push(trace);
    groups.set(trace.user_id, rows);
  }
  return [...groups.entries()].map(([userId, rows]) => {
    const start = rows.reduce(
      (earliest, trace) => Math.min(earliest, Date.parse(trace.start_time)),
      Infinity,
    );
    const end = rows.reduce((latest, trace) => Math.max(latest, Date.parse(trace.end_time)), 0);
    const sessions = new Set(rows.flatMap((trace) => (trace.session_id ? [trace.session_id] : [])));
    const metadata = userMetadata(latestTrace(rows).metadata);
    return {
      user: {
        user_id: userId,
        first_seen: rfc3339(start),
        last_seen: rfc3339(end),
        trace_count: rows.length,
        session_count: sessions.size,
        error_count: rows.reduce((sum, trace) => sum + trace.error_count, 0),
        error_trace_count: rows.filter((trace) => trace.status === "error").length,
        cost_total: rows.reduce((sum, trace) => sum + (trace.cost_total ?? 0), 0),
        total_tokens: rows.reduce((sum, trace) => sum + (trace.total_tokens ?? 0), 0),
        scores: userScores(rows),
        ...(metadata ? { metadata } : {}),
      },
      traces: rows,
    };
  });
}

/**
 * Full user rows for a response. The window is the request's `[from, to)`, or without one
 * `[min first_seen, max last_seen + 1)` over these groups, as the server does (spec 09).
 */
export function userSummaries(
  groups: readonly UserGroup[],
  window: { from: number; to: number } | undefined,
): Schema<"EndUserSummary">[] {
  if (groups.length === 0) {
    return [];
  }
  const span = window ?? {
    from: groups.reduce(
      (earliest, group) => Math.min(earliest, Date.parse(group.user.first_seen)),
      Infinity,
    ),
    to:
      groups.reduce(
        (latest, group) => Math.max(latest, Date.parse(group.user.last_seen)),
        -Infinity,
      ) + 1,
  };
  return groups.map((group) => ({ ...group.user, activity: activityOf(group.traces, span) }));
}

function activityOf(
  traces: readonly Schema<"TraceSummary">[],
  window: { from: number; to: number },
): number[] {
  const width = Math.max(1, Math.ceil((window.to - window.from) / ACTIVITY_BUCKETS));
  const buckets = Array.from({ length: ACTIVITY_BUCKETS }, () => 0);
  for (const trace of traces) {
    const slot = Math.min(
      ACTIVITY_BUCKETS - 1,
      Math.max(0, Math.floor((Date.parse(trace.start_time) - window.from) / width)),
    );
    buckets[slot] = (buckets[slot] ?? 0) + 1;
  }
  return buckets;
}

/** The latest trace: greatest start, ties by greatest trace id. The group is never empty. */
function latestTrace(rows: readonly Schema<"TraceSummary">[]): Schema<"TraceSummary"> {
  return rows.reduce((best, trace) => {
    const order = Date.parse(trace.start_time) - Date.parse(best.start_time);
    return order > 0 || (order === 0 && trace.trace_id > best.trace_id) ? trace : best;
  });
}

function userMetadata(metadata: Record<string, string>): Record<string, string> | undefined {
  const keys = Object.keys(metadata)
    .filter((key) => key.startsWith("user."))
    .sort()
    .slice(0, USER_METADATA_LIMIT);
  if (keys.length === 0) {
    return undefined;
  }
  return Object.fromEntries(keys.map((key) => [key.slice("user.".length), metadata[key] ?? ""]));
}

function userScores(rows: readonly Schema<"TraceSummary">[]): Schema<"UserScore">[] {
  const groups = new Map<
    string,
    { name: string; data_type: "numeric" | "boolean"; sum: number; count: number }
  >();
  for (const trace of rows) {
    for (const score of trace.scores) {
      const { value } = score;
      if (value.data_type !== "numeric" && value.data_type !== "boolean") {
        continue;
      }
      const number = value.data_type === "boolean" ? (value.value ? 1 : 0) : value.value;
      const key = JSON.stringify([score.name, value.data_type]);
      const group = groups.get(key) ?? {
        name: score.name,
        data_type: value.data_type,
        sum: 0,
        count: 0,
      };
      group.sum += number;
      group.count += 1;
      groups.set(key, group);
    }
  }
  return [...groups.values()]
    .sort((left, right) =>
      left.name === right.name
        ? left.data_type.localeCompare(right.data_type)
        : left.name.localeCompare(right.name),
    )
    .map((group) => ({
      name: group.name,
      data_type: group.data_type,
      avg: group.sum / group.count,
      count: group.count,
    }));
}

export function scoreNumber(score: Schema<"ScoreBrief">): number | undefined {
  if (score.value.data_type === "numeric") {
    return score.value.value;
  }
  if (score.value.data_type === "boolean") {
    return score.value.value ? 1 : 0;
  }
  if (score.value.data_type === "categorical") {
    return score.value.value.value;
  }
  return undefined;
}

export function percentile(values: readonly number[], p: number): number | undefined {
  if (values.length === 0) {
    return undefined;
  }
  const sorted = [...values].sort((left, right) => left - right);
  const rank = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[rank];
}

export function bucketWidth(
  bucket: Schema<"MetricsBucket"> | undefined,
  from: number,
  to: number,
): number {
  if (bucket && bucket !== "auto") {
    return BUCKET_MS[bucket];
  }
  const span = Math.max(1, to - from);
  const widths = Object.values(BUCKET_MS);
  return widths.find((width) => span / width <= 120) ?? DAY_MS;
}

function kindAt(index: number, profile: Profile): Schema<"SpanKind"> {
  if (index === 0) {
    return "agent";
  }
  return profile.kinds[index % profile.kinds.length] ?? "span";
}

function llmCount(spanCount: number, profile: Profile): number {
  let count = 0;
  for (let index = 0; index < spanCount; index += 1) {
    if (kindAt(index, profile) === "llm") {
      count += 1;
    }
  }
  return count;
}

function errorSpan(index: number, spanCount: number, errorCount: number): boolean {
  if (errorCount <= 0) {
    return false;
  }
  if (spanCount === 1) {
    return true;
  }
  return index > 0 && index <= errorCount;
}

function environmentOf(rng: () => number): string {
  const roll = rng();
  if (roll < 0.03) {
    return "cetra-eval";
  }
  if (roll < 0.28) {
    return "staging";
  }
  return "production";
}

function ageOf(rng: () => number, index: number, profile: Profile): number {
  if (profile.bigFirstTrace && index === 0) {
    return 30_000;
  }
  if (rng() < 0.55) {
    return 60_000 + Math.floor(rng() * DAY_MS);
  }
  return DAY_MS + Math.floor(rng() * 29 * DAY_MS);
}

/** A trace's tags: the profile's base tags, then 0 to 2 drawn from its pool. */
function pickTags(rng: () => number, profile: Profile): string[] {
  const count = Math.floor(rng() * 3);
  const tags = new Set<string>(profile.tags.base);
  for (let index = 0; index < count; index += 1) {
    const tag = profile.tags.pool[Math.floor(rng() * profile.tags.pool.length)];
    if (tag) {
      tags.add(tag);
    }
  }
  return [...tags].sort();
}

/** A trace's session under its profile: a pool share, the call's session, or none. */
function sessionOf(
  sessions: Sessions,
  roll: number,
  rng: () => number,
  call: CallIds | undefined,
): string | undefined {
  switch (sessions.kind) {
    case "pool":
      return roll < sessions.share
        ? `${sessions.prefix}${String(Math.floor(rng() * sessions.pool)).padStart(4, "0")}`
        : undefined;
    case "calls":
      return call?.session;
    case "none":
      return undefined;
  }
}

/** A trace's user under its profile: a pool share, the call's user, or none. */
function userOf(
  users: Users,
  roll: number,
  rng: () => number,
  call: CallIds | undefined,
): string | undefined {
  switch (users.kind) {
    case "pool":
      return roll < users.share
        ? `${users.prefix}${String(Math.floor(rng() * users.pool)).padStart(3, "0")}`
        : undefined;
    case "per-call":
      return call?.user;
    case "none":
      return undefined;
  }
}

function scoresFor(index: number): Schema<"ScoreBrief">[] {
  return [
    {
      name: "correctness",
      score_id: scoreUuid(index, 0),
      source: "llm_judge",
      value: { data_type: "numeric", value: ((index % 10) + 1) / 10 },
    },
    {
      name: "relevance",
      score_id: scoreUuid(index, 1),
      source: "human",
      value: { data_type: "boolean", value: index % 4 !== 0 },
    },
  ];
}

export function scoreUuid(index: number, which: number): string {
  const tail = (index * 2 + which).toString(16).padStart(12, "0");
  return `00000000-0000-7000-8000-${tail}`;
}

/** Id of a score a mark adds: never equal to a `scoreUuid` (M17). */
export function markScoreUuid(index: number, position: number): string {
  return `00000000-0000-7000-9000-${(index * 8 + position).toString(16).padStart(12, "0")}`;
}

export function spanId(traceIndex: number, spanIndex: number): string {
  return `${traceIndex.toString(16).padStart(8, "0")}${(spanIndex + 1).toString(16).padStart(8, "0")}`;
}

function spanIndex(spanIdValue: string): number {
  return Number.parseInt(spanIdValue.slice(8), 16) - 1;
}

function textPayload(value: string): Schema<"Payload"> {
  return { format: "text", value };
}

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

export function rfc3339(ms: number): string {
  return new Date(ms)
    .toISOString()
    .replace(/\.(\d{3})Z$/, (_match, fraction: string) => `.${fraction}000000Z`);
}

export function mix(seed: number, index: number): number {
  return Math.imul(seed ^ index, 0x9e3779b1) >>> 0;
}
