import {
  CODING_SHOWCASE,
  EXTRACTION_SHOWCASE,
  mix,
  type Profile,
  scoreUuid,
  VOICE_SHOWCASE,
} from "./dataset";
import { mulberry32 } from "./random";

/**
 * The repositories of the coding agent's runs, as `cetra agents setup` reports them (`metadata.repository`), with each
 * one's weight a week ago and today: `chromium` grows from a fifth of the runs to almost half. The homepage's coding-agents
 * card shows their cost per day.
 */
export const CODING_REPOSITORIES: readonly (readonly [string, number, number])[] = [
  ["github.com/sebastiencs/chromium", 0.18, 0.48],
  ["github.com/sebastiencs/linux", 0.26, 0.22],
  ["github.com/sebastiencs/kubernetes", 0.22, 0.14],
  ["github.com/sebastiencs/postgres", 0.2, 0.1],
  ["github.com/sebastiencs/react", 0.14, 0.06],
];

/** The coding agents of those runs (`metadata.coding_agent`), with their shares. */
export const CODING_AGENTS: readonly (readonly [string, number])[] = [
  ["claude-code", 0.6],
  ["codex", 0.25],
  ["gemini-cli", 0.15],
];

/** Salt of the repository and agent draws, so they differ from every other draw of the generator. */
const CODING_METADATA_SALT = 0x3c6ef372;
const DAY_MS = 86_400_000;
/** The days the adoption ramp spans: the homepage card shows the last 7 days. */
const RAMP_DAYS = 7;

/** The value whose cumulative weight first exceeds `roll` times the total weight. */
function pickWeighted(choices: readonly (readonly [string, number])[], roll: number): string {
  const total = choices.reduce((sum, [, weight]) => sum + weight, 0);
  let cumulative = 0;
  for (const [value, weight] of choices) {
    cumulative += weight;
    if (roll * total < cumulative) {
      return value;
    }
  }
  return choices[choices.length - 1]?.[0] ?? "";
}

/**
 * The share of runs `ageMs` old that come from a coding agent set up with `cetra agents setup`: adoption grows
 * from 30% a month ago to all of them a day ago. The generator puts 55% of its runs in the last day, so a run
 * under a day old carries a repository with a probability that keeps that day level with the one before.
 */
function taggedShare(ageMs: number): number {
  const days = ageMs / DAY_MS;
  if (days < 1) {
    return 0.03;
  }
  return 0.4 + 0.6 * Math.max(0, 1 - (days - 1) / (RAMP_DAYS - 1));
}

/** The repository and coding agent of the coding agent's run `index`, `ageMs` old; none for an untagged run. */
export function codingTraceMetadata(index: number, ageMs: number): Record<string, string> {
  const rng = mulberry32(mix(CODING_METADATA_SALT, index));
  if (rng() >= taggedShare(ageMs)) {
    return {};
  }
  const recent = Math.max(0, Math.min(1, 1 - ageMs / (RAMP_DAYS * DAY_MS)));
  const repositories = CODING_REPOSITORIES.map(
    ([name, then, now]) => [name, then + (now - then) * recent] as const,
  );
  return {
    repository: pickWeighted(repositories, rng()),
    coding_agent: pickWeighted(CODING_AGENTS, rng()),
  };
}

/** The coding agent's generation (M16): long runs with tool calls, failing tests and developer users. */
export const CODING_PROFILE: Profile = {
  service: "code-agent",
  provider: "anthropic",
  names: [
    "coding-agent",
    "coding-agent",
    "coding-agent",
    "coding-agent",
    "fix-issue",
    "fix-issue",
    "review-pr",
    "write-tests",
    "refactor",
    "triage-issue",
  ],
  models: [
    "claude-sonnet-4-5",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
    "claude-haiku-4-5",
    "claude-haiku-4-5",
    "gpt-4.1",
    "gpt-4.1",
  ],
  kinds: ["llm", "tool", "tool", "chain", "llm", "tool", "tool", "tool", "llm", "evaluator"],
  spanNames: {
    llm: ["plan", "write patch", "review patch"],
    tool: ["read_file", "search_code", "apply_patch", "run_tests", "git_diff"],
    chain: ["attempt"],
    evaluator: ["tests pass"],
  },
  tags: { base: [], pool: ["ci", "pr", "retry"] },
  spans: { min: 20, span: 181, maxDepth: 4 },
  duration: { min: 8_000, span: 82_000, skew: 1 },
  errorRate: 0.12,
  error: {
    message: "1 failed, 47 passed",
    stacktrace:
      "AssertionError: expected 3 columns, got 4\n    at test_export_csv (tests/test_export.py:41)",
  },
  tokens: { inMin: 10_000, inSpan: 290_000, outMin: 500, outSpan: 7_500 },
  price: { input: 3, output: 15 },
  sessions: { kind: "pool", share: 0.3, pool: 60, prefix: "task-" },
  turnGap: { min: 60_000, max: 1_800_000 },
  users: { kind: "pool", share: 1, pool: 12, prefix: "dev-", label: "Developer" },
  traceMetadata: codingTraceMetadata,
  bigFirstTrace: false,
  turns: [
    {
      question: "Fix the CSV export: fields that contain a comma break the columns (issue #412).",
      heading: "CSV export quoting",
      summary: "Patched 2 files; 48 tests pass.",
      steps: [
        "Quote fields that contain a comma or a quote in `export_csv`",
        "Add a test with a comma inside a field",
      ],
      followUps: [
        {
          question: "Also escape newlines inside fields.",
          summary: "Patched 1 file; 49 tests pass.",
          steps: [],
        },
        {
          question: "Run the full test suite again.",
          summary: "All 49 tests pass.",
          steps: [],
        },
      ],
    },
    {
      question: "Flaky test in test_auth_refresh (issue #398).",
      heading: "Flaky token refresh test",
      summary: "Fixed the race; 120 runs pass.",
      steps: [
        "Freeze the clock in `test_auth_refresh`",
        "Wait for the refresh task before asserting",
      ],
      followUps: [
        {
          question: "Does it still fail on CI?",
          summary: "It passed 120 runs in a row on CI.",
          steps: [],
        },
        {
          question: "Remove the retry decorator too.",
          summary: "Removed it; the test passes without retries.",
          steps: [],
        },
      ],
    },
    {
      question: "Add pagination to the /traces endpoint (issue #405).",
      heading: "Cursor pagination",
      summary: "Added cursor pagination; 52 tests pass.",
      steps: ["Add `cursor` and `limit` to `GET /traces`", "Return `next_cursor` in the response"],
      followUps: [
        {
          question: "Cap the limit at 1,000.",
          summary: "Capped; a larger limit returns 400.",
          steps: [],
        },
        {
          question: "Update the API docs.",
          summary: "Updated `docs/api/traces.md`.",
          steps: [],
        },
      ],
    },
    {
      question: "The nightly job crashes on empty batches (issue #423).",
      heading: "Empty batch crash",
      summary: "Stopped: 1 failed, 47 passed.",
      steps: [
        "Return early when the batch is empty",
        "`test_nightly_empty` still fails on the summary step",
      ],
      followUps: [
        {
          question: "Look at the summary step.",
          summary: "Patched 1 file; 48 tests pass.",
          steps: ["Skip the summary when there are no rows"],
        },
        {
          question: "Add a log line for empty batches.",
          summary: "Added `nightly: empty batch, skipped`.",
          steps: [],
        },
      ],
    },
  ],
  tool: (traceIndex) => ({
    arguments: { path: "src/export.py" },
    result: { ok: true, lines: 40 + (traceIndex % 60) },
  }),
  systemPrompt: "Fix the issue with the smallest patch. Run the tests before you stop.",
  failedAnswer:
    "Stopped: 1 failed, 47 passed.\n\n- The patch did not fix test_export_csv\n- Hand the issue back with the failing test",
  scores: (index, errored) =>
    index % 2 === 0
      ? [
          {
            name: "tests_pass",
            score_id: scoreUuid(index, 0),
            source: "check",
            value: { data_type: "boolean", value: !errored },
          },
        ]
      : [],
  showcases: [{ ...CODING_SHOWCASE, user: "dev-007" }],
};

/** The voice desk's generation (M16): short turns of the front desk's calls, one caller per call. */
export const VOICE_PROFILE: Profile = {
  service: "voice-agent",
  provider: "openai",
  names: [
    "voice-agent",
    "voice-agent",
    "voice-agent",
    "voice-agent",
    "voice-agent",
    "voice-agent",
    "voice-agent",
    "booking-agent",
    "faq-agent",
    "handoff",
  ],
  models: ["gpt-4.1-mini"],
  kinds: ["span", "span", "llm", "tool", "llm", "guardrail", "span"],
  spanNames: {
    span: ["detect end of turn", "speech to text", "text to speech"],
    llm: ["detect intent", "draft reply"],
    tool: ["check_availability", "book_slot", "cancel_booking", "get_opening_hours"],
    guardrail: ["safety check"],
  },
  tags: { base: ["voice"], pool: ["booking", "faq"] },
  spans: { min: 5, span: 5, maxDepth: 2 },
  duration: { min: 400, span: 3_600, skew: 3 },
  errorRate: 0.03,
  error: {
    message: "calendar API timeout",
    stacktrace:
      "TimeoutError: calendar API timeout after 3000 ms\n    at check_availability (tools/calendar.ts:58:11)",
  },
  tokens: { inMin: 300, inSpan: 900, outMin: 20, outSpan: 80 },
  price: { input: 0.4, output: 1.6 },
  sessions: { kind: "calls", min: 4, max: 12, prefix: "call-" },
  turnGap: { min: 5_000, max: 15_000 },
  users: { kind: "per-call", pool: 60, prefix: "caller-", label: "Caller" },
  bigFirstTrace: false,
  turns: [
    {
      question: "Hi, can I move my dentist appointment to Friday afternoon?",
      heading: "Reschedule",
      summary: "Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you?",
      steps: [],
      followUps: [
        {
          question: "4:30 works.",
          summary: "Done: Friday at 4:30 pm. You'll get a text to confirm.",
          steps: [],
        },
        {
          question: "Is it still with Doctor Lee?",
          summary: "Yes, with Doctor Lee.",
          steps: [],
        },
        {
          question: "Do I need to bring anything?",
          summary: "Just your insurance card.",
          steps: [],
        },
        {
          question: "Thanks, bye.",
          summary: "You're welcome. Goodbye!",
          steps: [],
        },
      ],
    },
    {
      question: "What time do you open on Saturday?",
      heading: "Opening hours",
      summary: "We open at 9 am on Saturday and close at 1 pm.",
      steps: [],
      followUps: [
        {
          question: "And on Sunday?",
          summary: "We're closed on Sunday.",
          steps: [],
        },
        {
          question: "Is there parking?",
          summary: "Yes, behind the building, free for patients.",
          steps: [],
        },
        {
          question: "Can I come without an appointment?",
          summary: "For emergencies, yes. Otherwise please book first.",
          steps: [],
        },
        {
          question: "Okay, thanks.",
          summary: "You're welcome. Have a good day!",
          steps: [],
        },
      ],
    },
    {
      question: "I need to cancel my cleaning next week.",
      heading: "Cancel",
      summary: "I see your cleaning on Tuesday at 10 am. Should I cancel it?",
      steps: [],
      followUps: [
        {
          question: "Yes, please.",
          summary: "It's cancelled. Would you like to book another time?",
          steps: [],
        },
        {
          question: "Is there a cancellation fee?",
          summary: "No, there's no fee with more than 24 hours' notice.",
          steps: [],
        },
        {
          question: "Maybe in two weeks.",
          summary: "I have Tuesday the 2nd at 10 am. Should I book it?",
          steps: [],
        },
        {
          question: "Not now, thanks.",
          summary: "No problem. Goodbye!",
          steps: [],
        },
      ],
    },
    {
      question: "Is Doctor Lee available on Monday morning?",
      heading: "Availability",
      summary: "Doctor Lee has 9:30 and 11 am free on Monday. Should I book one?",
      steps: [],
      followUps: [
        {
          question: "9:30, please.",
          summary: "Booked: Monday at 9:30 am with Doctor Lee.",
          steps: [],
        },
        {
          question: "How long will it take?",
          summary: "About 45 minutes.",
          steps: [],
        },
        {
          question: "Can you text me the address?",
          summary: "Sure, I've sent it to your phone.",
          steps: [],
        },
        {
          question: "That's all, thanks.",
          summary: "You're welcome. See you Monday!",
          steps: [],
        },
      ],
    },
  ],
  tool: (traceIndex) => ({
    arguments: { day: ["Monday", "Tuesday", "Friday"][traceIndex % 3] ?? "Friday" },
    result: { slots: ["9:30", "11:00", "16:30"] },
  }),
  systemPrompt:
    "You are the front desk of Bright Smile Dental. Answer in one or two short spoken sentences.",
  failedAnswer:
    "Sorry, I can't reach the calendar right now. Let me put you through to the front desk.",
  scores: () => [],
  showcases: [{ ...VOICE_SHOWCASE, user: "caller-019" }],
};

/** The invoice pipeline's generation (M16): a batch of cheap extractions, each checked against its schema. */
export const EXTRACTION_PROFILE: Profile = {
  service: "doc-pipeline",
  provider: "google",
  names: [
    "extract-invoice",
    "extract-invoice",
    "extract-invoice",
    "extract-invoice",
    "extract-invoice",
    "extract-invoice",
    "extract-invoice",
    "classify-document",
    "extract-receipt",
    "validate-invoice",
  ],
  models: [
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash",
    "claude-sonnet-4-5",
  ],
  kinds: ["tool", "tool", "llm", "evaluator", "tool", "llm"],
  spanNames: {
    tool: ["load_pdf", "ocr", "save_invoice"],
    llm: ["extract fields", "re-extract fields"],
    evaluator: ["validate schema"],
  },
  tags: { base: ["billing"], pool: ["batch", "retry"] },
  spans: { min: 6, span: 5, maxDepth: 2 },
  duration: { min: 2_000, span: 7_000, skew: 1 },
  errorRate: 0.05,
  error: {
    message: "schema check failed: total",
    stacktrace:
      "SchemaError: total does not match the line items\n    at validate (pipeline/validate.py:27)",
  },
  tokens: { inMin: 1_500, inSpan: 4_500, outMin: 200, outSpan: 600 },
  price: { input: 0.3, output: 2.5 },
  sessions: { kind: "none" },
  turnGap: { min: 30_000, max: 480_000 },
  users: { kind: "none" },
  bigFirstTrace: false,
  turns: [
    {
      question: "invoice-2026-0418.pdf (2 pages)",
      heading: "Alder Office Supply",
      summary: "Alder Office Supply · INV-2026-0418 · 2026-11-12 · 4 line items · 1,284.00 EUR",
      steps: [],
      followUps: [
        {
          question: "Extract again with the strong model.",
          summary: "Same fields; the total matches.",
          steps: [],
        },
        {
          question: "Validate the total.",
          summary: "The total matches the line items.",
          steps: [],
        },
      ],
    },
    {
      question: "invoice-2026-0421.pdf (1 page)",
      heading: "Brightline Logistics",
      summary: "Brightline Logistics · INV-2026-0421 · 2026-11-13 · 2 line items · 860.50 EUR",
      steps: [],
      followUps: [
        {
          question: "Extract again with the strong model.",
          summary: "Same fields; the total matches.",
          steps: [],
        },
        {
          question: "Validate the total.",
          summary: "The total matches the line items.",
          steps: [],
        },
      ],
    },
    {
      question: "invoice-2026-0425.pdf (3 pages)",
      heading: "Copperleaf Foods",
      summary: "Copperleaf Foods · INV-2026-0425 · 2026-11-14 · 11 line items · 3,912.40 EUR",
      steps: [],
      followUps: [
        {
          question: "Extract again with the strong model.",
          summary: "Same fields; the total matches.",
          steps: [],
        },
        {
          question: "Validate the total.",
          summary: "The total matches the line items.",
          steps: [],
        },
      ],
    },
    {
      question: "invoice-2026-0430.pdf (1 page)",
      heading: "Delta Print Works",
      summary: "Delta Print Works · INV-2026-0430 · 2026-11-15 · 3 line items · 412.00 EUR",
      steps: [],
      followUps: [
        {
          question: "Extract again with the strong model.",
          summary: "Same fields; the total matches.",
          steps: [],
        },
        {
          question: "Validate the total.",
          summary: "The total matches the line items.",
          steps: [],
        },
      ],
    },
    {
      question: "invoice-2026-0433.pdf (2 pages)",
      heading: "Evergreen Cleaning",
      summary: "Evergreen Cleaning · INV-2026-0433 · 2026-11-16 · 5 line items · 1,050.00 EUR",
      steps: [],
      followUps: [
        {
          question: "Extract again with the strong model.",
          summary: "Same fields; the total matches.",
          steps: [],
        },
        {
          question: "Validate the total.",
          summary: "The total matches the line items.",
          steps: [],
        },
      ],
    },
    {
      question: "invoice-2026-0437.pdf (1 page)",
      heading: "Fjord Parts",
      summary: "Fjord Parts · INV-2026-0437 · 2026-11-17 · 7 line items · 2,276.80 EUR",
      steps: [],
      followUps: [
        {
          question: "Extract again with the strong model.",
          summary: "Same fields; the total matches.",
          steps: [],
        },
        {
          question: "Validate the total.",
          summary: "The total matches the line items.",
          steps: [],
        },
      ],
    },
  ],
  tool: (traceIndex) => ({
    arguments: {
      file: `invoice-2026-${String(400 + (traceIndex % 600)).padStart(4, "0")}.pdf`,
    },
    result: { pages: 1 + (traceIndex % 3) },
  }),
  systemPrompt:
    "Extract the invoice fields as JSON: vendor, invoice_number, date, line_items, total, currency.",
  failedAnswer: "Schema check failed twice: total does not match the line items.",
  scores: (index, errored) => [
    {
      name: "schema_valid",
      score_id: scoreUuid(index, 0),
      source: "check",
      value: { data_type: "boolean", value: !errored },
    },
  ],
  showcases: [EXTRACTION_SHOWCASE],
};
