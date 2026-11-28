import type { Schema } from "@/api/types";
import {
  type Dataset,
  isEligibleTrace,
  type Mark,
  type MarkedSpan,
  type MarksContext,
  mergeMarks,
  type Profile,
  type Showcase,
  SUPPORT_PROFILE,
  type Turn,
} from "./dataset";

type TraceSummary = Schema<"TraceSummary">;

const HOUR_MS = 3_600_000;
const DAY_MS = 86_400_000;
const WEEK_MS = 7 * DAY_MS;

/** "Since Tuesday 14:00" is `now - 46 h` (P-5). */
const SINCE_TUESDAY_MS = 46 * HOUR_MS;
/** The correctness dip starts at `now - 5 h` (D). */
const DIP_MS = 5 * HOUR_MS;

/** Every third generated trace gets a `user_feedback` score (P2 mark 2). */
const FEEDBACK_EVERY = 3;
const DOWNVOTES_SINCE_TUESDAY = 66;
const DOWNVOTES_QUOTING_OLD_PRICING = 41;
const SSO_THIS_WEEK = 214;
const SSO_PREVIOUS_WEEK = 30;

/** The provider's rate-limit burst: it starts this long before now (the shared Alerts picture, M19 retake). */
export const ERROR_BURST_MS = 12 * 60_000;
/** How many error-free traces of the burst fail. */
const ERROR_BURST_TRACES = 8;
const RATE_LIMIT_ERROR = {
  count: 1,
  spanName: "generate answer",
  message: "RateLimitError: 429 Too Many Requests from the model provider",
  stacktrace:
    "RateLimitError: 429 Too Many Requests from the model provider\n    at generate (agent/answer.ts:58:11)",
};

/** The release the support team shipped on Tuesday (P2 mark 1). */
const RELEASE = "v2.14";
/** The tag of the traces whose answer quotes the 2025 pricing (P2 mark 2). */
export const OLD_PRICING_TAG = "old-pricing";
/** The session of the hand-written conversation (P2 mark 5). */
export const SUPPORT_SESSION = "sess-0142";

/** The questions the SSO traces ask (P2 mark 3), in the order the marks cycle through them. */
export const SSO_QUESTIONS: readonly string[] = [
  "Do you have SSO on the Starter plan?",
  "Is SAML SSO included on Starter?",
  "Can we log in with Okta on the Starter plan?",
  "Do you support single sign-on below Team?",
  "We need Google Workspace SSO, which plan?",
];

/** The turn every SSO question shares: the answer, and its follow-ups. */
const SSO_BASE: Omit<Turn, "question"> = {
  heading: "SSO on the Starter plan",
  summary: "SSO comes with the Team plan. Starter has email and password, and Google sign-in.",
  steps: ["Upgrade to Team to turn on SAML or OIDC SSO", "Google sign-in works on every plan"],
  docs: [
    "SSO (SAML and OIDC) is part of the Team plan.",
    "Starter: email and password, Google sign-in.",
    "Team trial: 14 days.",
  ],
  followUps: [
    {
      question: "How much is Team?",
      summary: "Team is $99 per month for 5 seats.",
      steps: [],
    },
    {
      question: "Can I try it first?",
      summary: "Yes, Team has a 14-day trial.",
      steps: [],
    },
  ],
};

/** The SSO turns, one per question in `SSO_QUESTIONS` order: the same answer and follow-ups for each. */
export const SSO_TURNS: readonly Turn[] = SSO_QUESTIONS.map((question) => ({
  ...SSO_BASE,
  question,
}));

/** The retriever span a downvote that quotes the old pricing has (P2 mark 2). */
const RETRIEVE_OLD_PRICING: MarkedSpan = {
  name: "retrieve docs",
  kind: "retriever",
  fromMs: 300,
  durationMs: 180,
  output: {
    format: "json",
    value: {
      documents: [
        {
          id: "pricing-2025.md",
          score: 0.91,
          text: "Pro plan: $49 per seat per month (2025 pricing).",
        },
        { id: "plans.md", score: 0.74, text: "Compare the Starter, Pro and Team plans." },
      ],
    },
  },
};

const SUPPORT_AGENT_SYSTEM = "You are a concise support agent. Answer in Markdown.";

function textPayload(value: string): Schema<"Payload"> {
  return { format: "text", value };
}

function jsonPayload(value: unknown): Schema<"Payload"> {
  return { format: "json", value };
}

/** An llm span's payloads: a system and a user message in, one assistant message out. */
function chatPayloads(
  system: string,
  user: string,
  answer: string,
): { input: Schema<"Payload">; output: Schema<"Payload"> } {
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

const EXPORT_QUESTION = "How can I export my traces to a CSV file?";
const EXPORT_ANSWER =
  "Open Traces, filter the rows you want, then use Export → CSV. Exports run in the background and you get a link by email.";
const LIMIT_QUESTION = "What's the limit for one export?";
const LIMIT_ANSWER = "100,000 traces per export. Above that, use the API with a cursor.";
export const SCHEDULE_QUESTION = "Can I schedule it every week?";
export const SCHEDULE_ANSWER = "You can export any time from the Traces page with Export → CSV.";
const THANKS_QUESTION = "Okay, thanks.";
const THANKS_ANSWER = "You're welcome.";

/**
 * The conversation of session `sess-0142` (P2 mark 5): four turns of `support-bot` for `user-031`. Their
 * ages are the session's anchor; `spreadSessionTurns` keeps the first turn's start.
 */
export const SESSION_SHOWCASES: readonly Showcase[] = [
  {
    index: 7,
    ageMs: 1_500_000,
    service: "support-bot",
    user: "user-031",
    session: SUPPORT_SESSION,
    tags: ["email"],
    models: ["claude-sonnet-4-5"],
    scores: [
      { name: "correctness", source: "llm_judge", value: { data_type: "numeric", value: 0.92 } },
    ],
    spans: [
      { name: "support-agent", kind: "agent", from: 0, to: 4_000 },
      { name: "retrieve docs", kind: "retriever", parent: 0, from: 120, to: 420 },
      {
        name: "answer",
        kind: "llm",
        parent: 0,
        from: 450,
        to: 3_900,
        model: "claude-sonnet-4-5",
        provider: "anthropic",
        inputTokens: 1_820,
        outputTokens: 210,
        cost: 0.0086,
      },
    ],
    story: {
      inputPreview: EXPORT_QUESTION,
      outputPreview: EXPORT_ANSWER,
      payloads: [
        { input: textPayload(EXPORT_QUESTION), output: textPayload(EXPORT_ANSWER) },
        {
          input: jsonPayload({ query: EXPORT_QUESTION }),
          output: jsonPayload({
            documents: [
              { id: "export.md", text: "Exports run in the background and send a link by email." },
              { id: "limits.md", text: "One export holds up to 100,000 traces." },
            ],
          }),
        },
        chatPayloads(SUPPORT_AGENT_SYSTEM, EXPORT_QUESTION, EXPORT_ANSWER),
      ],
    },
  },
  {
    index: 8,
    ageMs: 1_380_000,
    service: "support-bot",
    user: "user-031",
    session: SUPPORT_SESSION,
    tags: ["email"],
    models: ["claude-sonnet-4-5"],
    scores: [
      { name: "correctness", source: "llm_judge", value: { data_type: "numeric", value: 0.95 } },
    ],
    spans: [
      { name: "support-agent", kind: "agent", from: 0, to: 2_200 },
      {
        name: "answer",
        kind: "llm",
        parent: 0,
        from: 80,
        to: 2_100,
        model: "claude-sonnet-4-5",
        provider: "anthropic",
        inputTokens: 1_960,
        outputTokens: 64,
        cost: 0.0068,
      },
    ],
    story: {
      inputPreview: LIMIT_QUESTION,
      outputPreview: LIMIT_ANSWER,
      payloads: [
        { input: textPayload(LIMIT_QUESTION), output: textPayload(LIMIT_ANSWER) },
        chatPayloads(SUPPORT_AGENT_SYSTEM, LIMIT_QUESTION, LIMIT_ANSWER),
      ],
    },
  },
  {
    index: 9,
    ageMs: 1_200_000,
    service: "support-bot",
    user: "user-031",
    session: SUPPORT_SESSION,
    tags: ["email"],
    models: ["claude-sonnet-4-5"],
    scores: [
      { name: "correctness", source: "llm_judge", value: { data_type: "numeric", value: 0.3 } },
      { name: "user_feedback", source: "end_user", value: { data_type: "boolean", value: false } },
    ],
    spans: [
      { name: "support-agent", kind: "agent", from: 0, to: 1_800 },
      {
        name: "answer",
        kind: "llm",
        parent: 0,
        from: 90,
        to: 1_700,
        model: "claude-sonnet-4-5",
        provider: "anthropic",
        inputTokens: 2_010,
        outputTokens: 41,
        cost: 0.0066,
      },
    ],
    story: {
      inputPreview: SCHEDULE_QUESTION,
      outputPreview: SCHEDULE_ANSWER,
      payloads: [
        { input: textPayload(SCHEDULE_QUESTION), output: textPayload(SCHEDULE_ANSWER) },
        chatPayloads(SUPPORT_AGENT_SYSTEM, SCHEDULE_QUESTION, SCHEDULE_ANSWER),
      ],
    },
  },
  {
    index: 10,
    ageMs: 1_080_000,
    service: "support-bot",
    user: "user-031",
    session: SUPPORT_SESSION,
    tags: ["email"],
    models: ["claude-sonnet-4-5"],
    scores: [],
    spans: [
      { name: "support-agent", kind: "agent", from: 0, to: 800 },
      {
        name: "answer",
        kind: "llm",
        parent: 0,
        from: 60,
        to: 700,
        model: "claude-sonnet-4-5",
        provider: "anthropic",
        inputTokens: 2_060,
        outputTokens: 8,
        cost: 0.0063,
      },
    ],
    story: {
      inputPreview: THANKS_QUESTION,
      outputPreview: THANKS_ANSWER,
      payloads: [
        { input: textPayload(THANKS_QUESTION), output: textPayload(THANKS_ANSWER) },
        chatPayloads(SUPPORT_AGENT_SYSTEM, THANKS_QUESTION, THANKS_ANSWER),
      ],
    },
  },
];

/** Adds a step's mark to a trace's mark, so one trace can carry the marks of several steps (M17 `mergeMarks`). */
function addMark(marks: Map<number, Mark>, index: number, mark: Mark): void {
  const current = marks.get(index);
  marks.set(index, current ? mergeMarks(current, mark) : mark);
}

/** Rounds to two decimals, so the dip's values are exact (0.42 + 3 x 0.06 is not). */
function round2(value: number): number {
  return Math.round(value * 100) / 100;
}

/**
 * The support story's marks (M17, P2). Marks choose only eligible traces, in index order: the first N by index
 * of a candidate set.
 */
function supportMarks(
  traces: readonly TraceSummary[],
  context: MarksContext,
): ReadonlyMap<number, Mark> {
  const marks = new Map<number, Mark>();
  const { eligible, now } = context;
  const indices = traces.map((_, index) => index).filter((index) => eligible(index));
  const start = (index: number): number => Date.parse(traces[index]?.start_time ?? "");
  const hasSession = (index: number): boolean => traces[index]?.session_id !== undefined;
  const since = now - SINCE_TUESDAY_MS;
  const dip = now - DIP_MS;

  // 1. The release: every generated trace from Tuesday 14:00 on.
  for (const index of indices) {
    if (start(index) >= since) {
      addMark(marks, index, { release: RELEASE });
    }
  }

  // 2. The feedback: the downvotes are the first 66 of those since Tuesday, and a share of those before.
  const feedback = indices.filter((index) => index % FEEDBACK_EVERY === 0);
  const feedbackAfter = feedback.filter((index) => start(index) >= since);
  const feedbackBefore = feedback.filter((index) => start(index) < since);
  const downvotesAfter = feedbackAfter.slice(0, DOWNVOTES_SINCE_TUESDAY);
  const downvotesBefore = feedbackBefore.slice(
    0,
    Math.round((feedbackBefore.length * (DOWNVOTES_SINCE_TUESDAY / feedbackAfter.length)) / 2),
  );
  const downvotes = new Set([...downvotesAfter, ...downvotesBefore]);
  for (const index of feedback) {
    addMark(marks, index, {
      scores: [
        {
          name: "user_feedback",
          source: "end_user",
          value: { data_type: "boolean", value: !downvotes.has(index) },
        },
      ],
    });
  }
  for (const index of downvotesAfter.slice(0, DOWNVOTES_QUOTING_OLD_PRICING)) {
    addMark(marks, index, { tags: [OLD_PRICING_TAG], spans: [RETRIEVE_OLD_PRICING] });
  }

  // 3. The SSO topic: the plain traces (no session, not a downvote) of the last two weeks. Their questions cycle
  //    through `SSO_TURNS` by rank, newest first, so the newest five traces ask five different questions.
  const plain = (index: number): boolean => !hasSession(index) && !downvotes.has(index);
  const thisWeek = indices
    .filter((index) => plain(index) && start(index) >= now - WEEK_MS && start(index) < now)
    .slice(0, SSO_THIS_WEEK);
  const previousWeek = indices
    .filter(
      (index) => plain(index) && start(index) >= now - 2 * WEEK_MS && start(index) < now - WEEK_MS,
    )
    .slice(0, SSO_PREVIOUS_WEEK);
  const ssoByNewest = [...thisWeek, ...previousWeek].sort(
    (left, right) => start(right) - start(left) || left - right,
  );
  ssoByNewest.forEach((index, rank) => {
    const turn = SSO_TURNS[rank % SSO_TURNS.length] ?? SSO_TURNS[0];
    if (turn) {
      addMark(marks, index, { turn });
    }
  });

  // 4. The correctness dip: every correctness score moves down from `now - 5 h`.
  for (const index of indices) {
    if (!traces[index]?.scores.some((score) => score.name === "correctness")) {
      continue;
    }
    const value = start(index) < dip ? 0.82 + (index % 4) * 0.04 : 0.42 + (index % 4) * 0.06;
    addMark(marks, index, {
      scores: [
        {
          name: "correctness",
          source: "llm_judge",
          value: { data_type: "numeric", value: round2(value) },
        },
      ],
    });
  }

  // 5. The rate-limit burst: the first error-free traces of the last 12 minutes fail.
  const burst = indices
    .filter(
      (index) =>
        start(index) >= now - ERROR_BURST_MS &&
        start(index) < now &&
        traces[index]?.error_count === 0,
    )
    .slice(0, ERROR_BURST_TRACES);
  for (const index of burst) {
    addMark(marks, index, { error: RATE_LIMIT_ERROR });
  }

  return marks;
}

/** The support story's profile: the M16 support profile with the conversation and the marks (M17, P2). */
export const SUPPORT_STORY_PROFILE: Profile = {
  ...SUPPORT_PROFILE,
  showcases: [...SUPPORT_PROFILE.showcases, ...SESSION_SHOWCASES],
  marks: supportMarks,
};

export type SupportFacts = {
  /** Downvotes since Tuesday 14:00 (eligible traces). */
  downvotesSinceTuesday: number;
  /** Traces that carry the old-pricing tag. */
  quotingOldPricing: number;
  /** The share of the downvotes that quote the old pricing (41 / 66). */
  quotingShare: number;
  /** Downvotes over feedback traces, before Tuesday 14:00. */
  downvoteRateBefore: number;
  /** Downvotes over feedback traces, since Tuesday 14:00. */
  downvoteRateAfter: number;
  /** Traces that ask about SSO in the last seven days. */
  ssoThisWeek: number;
  /** Traces that ask about SSO in the seven days before. */
  ssoPreviousWeek: number;
  /** Mean correctness of the traces before the dip. */
  correctnessBeforeDip: number;
  /** Mean correctness of the traces from the dip on. */
  correctnessAfterDip: number;
  /**
   * The Explore picture's comparison (M19 retake): the eligible traces of the week before Tuesday 14:00 and
   * those since, with their mean correctness, the downvotes over the feedback traces and the mean cost.
   */
  weekBeforeTuesday: ReleaseWindow;
  sinceTuesday: ReleaseWindow;
  /** The turns of `sess-0142` in time order. */
  sessionTurns: readonly {
    input: string;
    correctness: number | undefined;
    userFeedback: boolean | undefined;
  }[];
};

export type ReleaseWindow = {
  answers: number;
  correctness: number;
  downvoteRate: number;
  costPerAnswer: number;
};

function numericScore(trace: TraceSummary, name: string): number | undefined {
  const value = trace.scores.find((score) => score.name === name)?.value;
  return value?.data_type === "numeric" ? value.value : undefined;
}

function booleanScore(trace: TraceSummary, name: string): boolean | undefined {
  const value = trace.scores.find((score) => score.name === name)?.value;
  return value?.data_type === "boolean" ? value.value : undefined;
}

function mean(values: readonly number[]): number {
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

/**
 * The numbers of the support story's pictures (M17, P2), from a dataset built with `SUPPORT_STORY_PROFILE`.
 * Counts cover the eligible traces only: not a showcase, not the big first trace, and visible.
 */
export function supportFacts(dataset: Dataset): SupportFacts {
  const now = dataset.now;
  const since = now - SINCE_TUESDAY_MS;
  const dip = now - DIP_MS;
  // The index is the dataset's order, so the facts never read it from a trace id.
  const eligible = dataset.traces.filter((trace, index) =>
    isEligibleTrace(SUPPORT_STORY_PROFILE, trace, index),
  );
  const start = (trace: TraceSummary): number => Date.parse(trace.start_time);
  const feedback = eligible.filter((trace) => booleanScore(trace, "user_feedback") !== undefined);
  const feedbackAfter = feedback.filter((trace) => start(trace) >= since);
  const feedbackBefore = feedback.filter((trace) => start(trace) < since);
  const downAfter = feedbackAfter.filter((trace) => booleanScore(trace, "user_feedback") === false);
  const downBefore = feedbackBefore.filter(
    (trace) => booleanScore(trace, "user_feedback") === false,
  );
  const quoting = eligible.filter((trace) => trace.tags.includes(OLD_PRICING_TAG));
  const downvotesSinceTuesday = downAfter.length;
  const quotingOldPricing = quoting.length;
  const weekStart = now - WEEK_MS;
  const twoWeeksStart = now - 2 * WEEK_MS;
  const sso = (from: number, to: number): number =>
    eligible.filter(
      (trace) =>
        SSO_QUESTIONS.some((question) => trace.input_preview === question) &&
        start(trace) >= from &&
        start(trace) < to,
    ).length;
  const correctness = (from: number, to: number): number[] =>
    eligible.flatMap((trace) => {
      const value = numericScore(trace, "correctness");
      return value !== undefined && start(trace) >= from && start(trace) < to ? [value] : [];
    });
  const session = dataset.session(SUPPORT_SESSION, false, false);
  const windowOf = (from: number, to: number): ReleaseWindow => {
    const traces = eligible.filter((trace) => start(trace) >= from && start(trace) < to);
    const rated = traces.filter((trace) => booleanScore(trace, "user_feedback") !== undefined);
    return {
      answers: traces.length,
      correctness: mean(correctness(from, to)),
      downvoteRate:
        rated.filter((trace) => booleanScore(trace, "user_feedback") === false).length /
        rated.length,
      costPerAnswer: mean(traces.map((trace) => trace.cost_total ?? 0)),
    };
  };

  return {
    downvotesSinceTuesday,
    quotingOldPricing,
    quotingShare: quotingOldPricing / downvotesSinceTuesday,
    downvoteRateBefore: downBefore.length / feedbackBefore.length,
    downvoteRateAfter: downvotesSinceTuesday / feedbackAfter.length,
    ssoThisWeek: sso(weekStart, now),
    ssoPreviousWeek: sso(twoWeeksStart, weekStart),
    correctnessBeforeDip: mean(correctness(0, dip)),
    correctnessAfterDip: mean(correctness(dip, Number.POSITIVE_INFINITY)),
    weekBeforeTuesday: windowOf(since - WEEK_MS, since),
    sinceTuesday: windowOf(since, Number.POSITIVE_INFINITY),
    sessionTurns: (session?.turns ?? []).map(({ trace }) => ({
      input: trace.input_preview ?? "",
      correctness: numericScore(trace, "correctness"),
      userFeedback: booleanScore(trace, "user_feedback"),
    })),
  };
}
