import type { Schema } from "@/api/types";
import { mulberry32 } from "@/mocks/generated/random";

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
  /** With `userId`, only that user's traces count (UP-5, UP-9). */
  metrics(
    from: number,
    to: number,
    bucket: Schema<"MetricsBucket"> | undefined,
    includeEval: boolean,
    includeExperiments: boolean,
    userId: string | undefined,
  ): Schema<"MetricsResponse">;
};

export function createDataset(options: { seed: number; now: number; traces: number }): Dataset {
  const bases: Schema<"TraceSummary">[] = [];
  for (let index = 0; index < options.traces; index += 1) {
    bases.push(traceSummary(options.seed, index, options.now));
  }
  // A session's turns are spread out in time first: the session's turns depend on each other.
  const timed = spreadSessionTurns(bases, options.seed, options.now);
  // A trace's turn depends on its session's other traces, so the previews are set once all are known.
  const slots = sessionSlots(timed);
  const turnAt = (index: number): TraceTurn => {
    const slot = slots.get(index);
    return turnOf(slot?.topic ?? index, slot?.turn ?? 1);
  };
  const traces = timed.map((trace, index) => withPreviews(trace, turnAt(index)));
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
    const detail = buildDetail(options.seed, index, trace);
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
      return spanFromSummary(row.span, {
        traceIndex: index,
        errored: detail.trace.error_count > 0,
        turn: turnAt(index),
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
    metrics(from, to, bucket, includeEval, includeExperiments, userId) {
      const scoped =
        userId === undefined ? traces : traces.filter((trace) => trace.user_id === userId);
      return metricsFrom(
        visibleTraces(scoped, includeEval, includeExperiments),
        scoped.filter((trace) => trace.environment === "cetra-eval"),
        from,
        to,
        bucket,
      );
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

function traceSummary(seed: number, index: number, now: number): Schema<"TraceSummary"> {
  const rng = mulberry32(mix(seed, index));
  const spanCount = index === 0 ? 10_000 : 3 + Math.floor(rng() * 58);
  const errored = rng() < 0.08;
  const errorCount = errored ? Math.min(spanCount, 1 + Math.floor(rng() * 3)) : 0;
  const environment = environmentOf(rng);
  const experiment = environment !== "cetra-eval" && rng() < 0.04;
  const ageMs = ageOf(rng, index);
  const durationMs = index === 0 ? 60_000 : 200 + Math.floor(rng() * 20_000);
  const startMs = now - ageMs;
  const name = NAMES[Math.floor(rng() * NAMES.length)] ?? NAMES[0];
  const model = MODELS[Math.floor(rng() * MODELS.length)] ?? MODELS[0];
  const inputTokens = 100 + Math.floor(rng() * 1_900);
  const outputTokens = 20 + Math.floor(rng() * 400);
  const session =
    rng() < 0.5 ? `sess-${String(Math.floor(rng() * 120)).padStart(4, "0")}` : undefined;
  const user = rng() < 0.6 ? `user-${String(Math.floor(rng() * 40)).padStart(3, "0")}` : undefined;
  const tags = pickTags(rng);
  const metadata: Record<string, string> = {};
  if (experiment) {
    metadata["cetra.experiment_id"] = `exp-${String(index % 12).padStart(2, "0")}`;
  }
  if (user) {
    Object.assign(metadata, userTraceMetadata(user));
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
    llm_count: llmCount(spanCount),
    input_tokens: inputTokens,
    output_tokens: outputTokens,
    total_tokens: inputTokens + outputTokens,
    cost_total: Math.round(inputTokens * 3 + outputTokens * 15) / 1_000_000,
    models: [model],
    scores: index % 2 === 0 ? scoresFor(index) : [],
    ...(session ? { session_id: session } : {}),
    ...(user ? { user_id: user } : {}),
  };
}

/** `user.*` metadata of a user's traces, derived from the id `user-NNN` so it is stable. Values are JSON-encoded (spec 01). */
function userTraceMetadata(userId: string): Record<string, string> {
  const index = Number.parseInt(userId.slice("user-".length), 10);
  return {
    "user.name": JSON.stringify(`User ${userId.slice("user-".length)}`),
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
): Schema<"TraceDetail"> {
  const rng = mulberry32(mix(seed, index + 17));
  const spanCount = trace.span_count;
  const maxDepth = index === 0 ? 8 : 6;
  const shape = spanShape(spanCount, maxDepth, index === 0 ? 2_000 : 0, rng);
  const startMs = Date.parse(trace.start_time);
  const endMs = Date.parse(trace.end_time);
  const built = layoutSpans(shape, startMs, endMs, trace.error_count, rng);
  const spans = built.map((span, spanIndex) => treeSpan(span, spanIndex, index, trace));
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
): BuiltSpan[] {
  const built: BuiltSpan[] = [];
  const stack: number[] = [];
  const lastChild = new Map<number, number>();
  for (let index = 0; index < shape.length; index += 1) {
    const row = shape[index];
    if (!row) {
      continue;
    }
    const kind = kindAt(index);
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
): Schema<"TreeSpan"> {
  const id = spanId(traceIndex, spanIndex);
  const summary: Schema<"SpanSummary"> = {
    span_id: id,
    trace_id: trace.trace_id,
    span_kind: span.kind,
    name: spanIndex === 0 ? trace.name : `${span.kind} ${spanIndex}`,
    start_time: rfc3339(span.startMs),
    end_time: rfc3339(span.endMs),
    duration_ns: (span.endMs - span.startMs) * 1_000_000,
    status: span.status,
    metadata: { ...trace.metadata },
    tags: trace.tags,
    ...(span.status === "error" ? { status_message: "tool failed" } : {}),
    ...(span.parent === undefined ? {} : { parent_span_id: spanId(traceIndex, span.parent) }),
    ...(trace.environment ? { environment: trace.environment } : {}),
    service_name: "support-bot",
    ...(trace.session_id ? { session_id: trace.session_id } : {}),
    ...(trace.user_id ? { user_id: trace.user_id } : {}),
    ...(trace.release ? { release: trace.release } : {}),
    ...(span.kind === "llm" && trace.models[0]
      ? { model: trace.models[0], provider: "anthropic" }
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

type SupportTurn = Exchange & {
  heading: string;
  snippets: { python: string; ts: string };
  docs: readonly [string, string, string];
  followUps: readonly [Exchange, Exchange, ...Exchange[]];
};

/** The question a trace answers: turn 1 of a session is the topic's question, later turns its follow-ups. */
type TraceTurn = { topic: SupportTurn; exchange: Exchange; main: boolean };

type SpanContext = { traceIndex: number; errored: boolean; turn: TraceTurn };

const FENCE = "```";
const FAILED_ANSWER =
  "I could not finish.\n\n- The lookup tool failed\n- Escalate to a human agent";
const TOOL_NAMES = ["lookup_ticket", "get_customer", "search_orders"] as const;
const RETRIEVER_SCORES = [0.92, 0.81, 0.67] as const;
const RERANK_SCORES = [0.52, 0.91, 0.84] as const;

export const SUPPORT_TURNS: readonly [SupportTurn, ...SupportTurn[]] = [
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

/**
 * Spreads each session's turns in time (SL-6): the session keeps its earliest trace's start, and each later
 * turn (in start-time order) starts 30 s to 8 min after the previous one. A session whose last turn would
 * start after `now` moves earlier as a whole. Traces keep their own durations.
 */
function spreadSessionTurns(
  traces: readonly Schema<"TraceSummary">[],
  seed: number,
  now: number,
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
    // Start-time order, ties by index: the same order `sessionSlots` numbers the turns in.
    const ordered = [...indices].sort(
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
      previous += TURN_GAP_MIN_MS + Math.floor(random() * (TURN_GAP_MAX_MS - TURN_GAP_MIN_MS + 1));
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
function turnOf(topicIndex: number, turn: number): TraceTurn {
  const topic = pick(SUPPORT_TURNS, topicIndex);
  if (turn <= 1) {
    return { topic, exchange: topic, main: true };
  }
  return { topic, exchange: pick(topic.followUps, turn - 2), main: false };
}

function withPreviews(trace: Schema<"TraceSummary">, turn: TraceTurn): Schema<"TraceSummary"> {
  return {
    ...trace,
    input_preview: turn.exchange.question,
    output_preview: firstLine(trace.error_count > 0 ? FAILED_ANSWER : shortAnswer(turn.exchange)),
  };
}

function firstLine(text: string): string {
  return text.split("\n")[0] ?? "";
}

function shortAnswer(turn: Exchange): string {
  return [turn.summary, "", ...turn.steps.map((step) => `- ${step}`)].join("\n");
}

function llmAnswer(turn: SupportTurn, language: "python" | "ts"): string {
  return [
    `## ${turn.heading}`,
    "",
    turn.summary,
    "",
    ...turn.steps.map((step) => `- ${step}`),
    "",
    `${FENCE}${language}`,
    turn.snippets[language],
    FENCE,
  ].join("\n");
}

function scoredDocs(
  turn: SupportTurn,
  scores: readonly [number, number, number],
): { id: string; score: number; text: string }[] {
  return turn.docs.map((text, i) => ({ id: `doc-${i + 1}`, score: scores[i] ?? 0, text }));
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
                "exception.stacktrace": "Error: tool failed\n    at lookup (agent.ts:12:5)",
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
  if (span.span_kind === "llm") {
    const language = index % 2 === 0 ? "python" : "ts";
    detail.input = {
      format: "messages",
      value: [
        {
          role: "system",
          parts: [
            {
              type: "text",
              content: "You are a concise support agent. Answer in Markdown.",
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
    const name = pick(TOOL_NAMES, index);
    detail.input = {
      format: "json",
      value: { name, arguments: { ticket_id: `T-${1000 + context.traceIndex}` } },
    };
    detail.output = {
      format: "json",
      value: { ticket_id: `T-${1000 + context.traceIndex}`, status: "open", assignee: "queue-2" },
    };
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
      value: { query: exchange.question, documents: turn.docs.map((_doc, i) => `doc-${i + 1}`) },
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
      value: failed ? FAILED_ANSWER : shortAnswer(exchange),
    };
  }
  return detail;
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

function metricsFrom(
  traces: readonly Schema<"TraceSummary">[],
  evalTraces: readonly Schema<"TraceSummary">[],
  from: number,
  to: number,
  bucket: Schema<"MetricsBucket"> | undefined,
): Schema<"MetricsResponse"> {
  const widthMs = bucketWidth(bucket, from, to);
  const times: number[] = [];
  for (let time = from; time < to; time += widthMs) {
    times.push(time);
  }
  const inRange = traces.filter((trace) => {
    const start = Date.parse(trace.start_time);
    return start >= from && start < to;
  });
  const buckets = times.map(() => [] as Schema<"TraceSummary">[]);
  for (const trace of inRange) {
    const slot = Math.floor((Date.parse(trace.start_time) - from) / widthMs);
    buckets[slot]?.push(trace);
  }
  const previousFrom = from - (to - from);
  const previous = traces.filter((trace) => {
    const start = Date.parse(trace.start_time);
    return start >= previousFrom && start < from;
  });
  const series: Schema<"MetricSeries">[] = [
    seriesOf("traces", times, buckets, (rows) => rows.length),
    seriesOf(
      "errors",
      times,
      buckets,
      (rows) => rows.filter((trace) => trace.status === "error").length,
    ),
    seriesOf("error_rate", times, buckets, (rows) =>
      rows.length === 0
        ? undefined
        : rows.filter((trace) => trace.status === "error").length / rows.length,
    ),
    seriesOf("latency_p50", times, buckets, (rows) =>
      percentile(
        rows.map((trace) => trace.duration_ns),
        50,
      ),
    ),
    seriesOf("latency_p95", times, buckets, (rows) =>
      percentile(
        rows.map((trace) => trace.duration_ns),
        95,
      ),
    ),
    seriesOf("latency_p99", times, buckets, (rows) =>
      percentile(
        rows.map((trace) => trace.duration_ns),
        99,
      ),
    ),
    seriesOf("cost", times, buckets, (rows) => sum(rows, (trace) => trace.cost_total ?? 0)),
    seriesOf("input_tokens", times, buckets, (rows) =>
      sum(rows, (trace) => trace.input_tokens ?? 0),
    ),
    seriesOf("output_tokens", times, buckets, (rows) =>
      sum(rows, (trace) => trace.output_tokens ?? 0),
    ),
    ...modelSeries(times, buckets),
    ...scoreSeries(times, buckets),
    seriesOf(
      "eval_cost",
      times,
      times.map((time) =>
        evalTraces.filter((trace) => {
          const start = Date.parse(trace.start_time);
          return start >= time && start < time + widthMs && start < to;
        }),
      ),
      (rows) => sum(rows, (trace) => trace.cost_total ?? 0),
    ),
  ];
  return {
    from: rfc3339(from),
    to: rfc3339(to),
    bucket_ns: widthMs * 1_000_000,
    kpis: kpis(inRange, previous),
    series,
    top_models: topModels(inRange),
  };
}

function seriesOf(
  metric: Schema<"MetricName">,
  times: readonly number[],
  buckets: readonly Schema<"TraceSummary">[][],
  value: (rows: readonly Schema<"TraceSummary">[]) => number | undefined,
  group?: string,
): Schema<"MetricSeries"> {
  return {
    metric,
    ...(group ? { group } : {}),
    points: times.map((time, index) => {
      const next = value(buckets[index] ?? []);
      return next === undefined ? { time: rfc3339(time) } : { time: rfc3339(time), value: next };
    }),
  };
}

function modelSeries(
  times: readonly number[],
  buckets: readonly Schema<"TraceSummary">[][],
): Schema<"MetricSeries">[] {
  const models = new Set<string>();
  for (const rows of buckets) {
    for (const trace of rows) {
      const model = trace.models[0];
      if (model) {
        models.add(model);
      }
    }
  }
  return [...models].sort().map((model) =>
    seriesOf(
      "cost_by_model",
      times,
      buckets,
      (rows) =>
        sum(
          rows.filter((trace) => trace.models[0] === model),
          (trace) => trace.cost_total ?? 0,
        ),
      model,
    ),
  );
}

function scoreSeries(
  times: readonly number[],
  buckets: readonly Schema<"TraceSummary">[][],
): Schema<"MetricSeries">[] {
  const names = new Set<string>();
  for (const rows of buckets) {
    for (const trace of rows) {
      for (const score of trace.scores) {
        names.add(score.name);
      }
    }
  }
  return [...names].sort().map((name) =>
    seriesOf(
      "score_avg",
      times,
      buckets,
      (rows) => {
        const values = rows.flatMap((trace) =>
          trace.scores.filter((score) => score.name === name).map(scoreNumber),
        );
        const numbers = values.filter((item): item is number => item !== undefined);
        if (numbers.length === 0) {
          return undefined;
        }
        return numbers.reduce((total, item) => total + item, 0) / numbers.length;
      },
      name,
    ),
  );
}

function kpis(
  current: readonly Schema<"TraceSummary">[],
  previous: readonly Schema<"TraceSummary">[],
): Schema<"Kpi">[] {
  const value = (rows: readonly Schema<"TraceSummary">[], metric: Schema<"KpiMetric">) => {
    if (rows.length === 0) {
      return undefined;
    }
    if (metric === "traces") {
      return rows.length;
    }
    if (metric === "error_rate") {
      return rows.filter((trace) => trace.status === "error").length / rows.length;
    }
    if (metric === "latency_p95") {
      return percentile(
        rows.map((trace) => trace.duration_ns),
        95,
      );
    }
    if (metric === "cost_total") {
      return sum(rows, (trace) => trace.cost_total ?? 0);
    }
    return sum(rows, (trace) => trace.total_tokens ?? 0);
  };
  const metrics: Schema<"KpiMetric">[] = [
    "traces",
    "error_rate",
    "latency_p95",
    "cost_total",
    "total_tokens",
  ];
  return metrics.map((metric) => {
    const currentValue = value(current, metric);
    const previousValue = value(previous, metric);
    return {
      metric,
      ...(currentValue === undefined ? {} : { value: currentValue }),
      ...(previousValue === undefined ? {} : { previous: previousValue }),
    };
  });
}

function topModels(traces: readonly Schema<"TraceSummary">[]): Schema<"ModelUsage">[] {
  const groups = new Map<string, { cost: number; tokens: number; llm: number }>();
  for (const trace of traces) {
    const model = trace.models[0];
    if (!model) {
      continue;
    }
    const row = groups.get(model) ?? { cost: 0, tokens: 0, llm: 0 };
    row.cost += trace.cost_total ?? 0;
    row.tokens += trace.total_tokens ?? 0;
    row.llm += trace.llm_count;
    groups.set(model, row);
  }
  return [...groups.entries()]
    .sort((left, right) => right[1].cost - left[1].cost)
    .slice(0, 10)
    .map(([model, row]) => ({
      model,
      llm_count: row.llm,
      cost_total: row.cost,
      total_tokens: row.tokens,
    }));
}

function scoreNumber(score: Schema<"ScoreBrief">): number | undefined {
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

function percentile(values: readonly number[], p: number): number | undefined {
  if (values.length === 0) {
    return undefined;
  }
  const sorted = [...values].sort((left, right) => left - right);
  const rank = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[rank];
}

function sum(
  rows: readonly Schema<"TraceSummary">[],
  value: (trace: Schema<"TraceSummary">) => number,
): number {
  return rows.reduce((total, trace) => total + value(trace), 0);
}

function bucketWidth(
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

function kindAt(index: number): Schema<"SpanKind"> {
  if (index === 0) {
    return "agent";
  }
  return KINDS[index % KINDS.length] ?? "span";
}

function llmCount(spanCount: number): number {
  let count = 0;
  for (let index = 0; index < spanCount; index += 1) {
    if (kindAt(index) === "llm") {
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

function ageOf(rng: () => number, index: number): number {
  if (index === 0) {
    return 30_000;
  }
  if (rng() < 0.55) {
    return 60_000 + Math.floor(rng() * DAY_MS);
  }
  return DAY_MS + Math.floor(rng() * 29 * DAY_MS);
}

function pickTags(rng: () => number): string[] {
  const count = Math.floor(rng() * 3);
  const tags = new Set<string>();
  for (let index = 0; index < count; index += 1) {
    const tag = TAGS[Math.floor(rng() * TAGS.length)];
    if (tag) {
      tags.add(tag);
    }
  }
  return [...tags].sort();
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

function scoreUuid(index: number, which: number): string {
  const tail = (index * 2 + which).toString(16).padStart(12, "0");
  return `00000000-0000-7000-8000-${tail}`;
}

function spanId(traceIndex: number, spanIndex: number): string {
  return `${traceIndex.toString(16).padStart(8, "0")}${(spanIndex + 1).toString(16).padStart(8, "0")}`;
}

function spanIndex(spanIdValue: string): number {
  return Number.parseInt(spanIdValue.slice(8), 16) - 1;
}

function textPayload(value: string): Schema<"Payload"> {
  return { format: "text", value };
}

function rfc3339(ms: number): string {
  return new Date(ms)
    .toISOString()
    .replace(/\.(\d{3})Z$/, (_match, fraction: string) => `.${fraction}000000Z`);
}

function mix(seed: number, index: number): number {
  return Math.imul(seed ^ index, 0x9e3779b1) >>> 0;
}
