import { type HttpHandler, HttpResponse, http } from "msw";
import assistantConversations from "../../../fixtures/api/responses/assistant-conversations.json";
import { turnStream } from "../test/turn-stream";
import { projectOfRef, type StoryId } from "./projects";
import { STORY_DASHBOARD_IDS, storyDashboards } from "./story-dashboards";

const PROJECT = "/api/v1/projects/:project/assistant";
const NEW_CONVERSATION_ID = "01900000-0000-7000-8000-000000001a02";
const TRACE_ID = "0af7651916cd43dd8448eb211c80319c";
const EVENT_DELAY_MS = 120;

/** One turn run in the new conversation: the script that answered and the context the request carried. */
type Ran = { script: Script; context: Fields | undefined };

/** Held in module state, as before; `resetAssistantMock()` clears it. */
let created = false;
let ran: Ran[] = [];

export function resetAssistantMock(): void {
  created = false;
  ran = [];
}

const newConversation = {
  id: NEW_CONVERSATION_ID,
  title: "",
  message_count: 0,
  created_at: "2026-11-19T12:00:00.000000000Z",
  last_active_at: "2026-11-19T12:00:00.000000000Z",
};

const scriptedTitle = "Which trace errors spiked?";

/** The events of the scripted turn, in the order the server sends them (spec 09 § Assistant). */
const scriptedEvents = [
  { event: "start", data: { turn_id: 1, trace_id: TRACE_ID } },
  { event: "text", data: { delta: "I'll search the failed traces " } },
  { event: "text", data: { delta: "from the last 24 hours. " } },
  {
    event: "tool_call",
    data: {
      id: "call_1",
      name: "search_traces",
      arguments: {
        from: "2026-01-01T00:00:00Z",
        to: "2026-01-02T00:00:00Z",
        filter: "status = 'error'",
      },
    },
  },
  { event: "tool_result", data: { id: "call_1", ok: true, summary: { size_bytes: 812, rows: 3 } } },
  {
    event: "tool_call",
    data: {
      id: "call_2",
      name: "draft_filter",
      arguments: {
        view: "traces",
        filter: "status = 'error'",
        range: "24h",
        include_eval: false,
        include_experiments: false,
      },
    },
  },
  { event: "tool_result", data: { id: "call_2", ok: true, summary: { size_bytes: 140 } } },
  {
    event: "draft",
    data: {
      id: "draft_call_2",
      kind: "filter",
      draft: {
        view: "traces",
        filter: "status = 'error'",
        range: "24h",
        include_eval: false,
        include_experiments: false,
      },
    },
  },
  {
    event: "text",
    data: {
      delta:
        "Three traces failed in the last 24 hours: 5f1c2a9e, 8b3d7f10 and c41e90aa. Open the filter to see them.",
    },
  },
  {
    event: "done",
    data: {
      finish_reason: "stop",
      usage: { input: 1200, output: 180 },
      cost_usd: 0.0021,
      steps: 3,
    },
  },
];

const scriptedToolRows = [
  { trace_id: "5f1c2a9e0000000000000000000000a1", name: "checkout", status: "error" },
  { trace_id: "8b3d7f100000000000000000000000b2", name: "checkout", status: "error" },
  { trace_id: "c41e90aa00000000000000000000c3c3", name: "payment", status: "error" },
];

/** The stored messages of the scripted turn, oldest first (D-AS-21 stored form). */
function scriptedMessages() {
  return [
    {
      seq: 1,
      role: "user",
      parts: [{ type: "text", content: "Which trace errors spiked in the last 24 hours?" }],
      context: { page: "traces", filter: "status = 'error'", range: "24h" },
      status: "complete",
      created_at: "2026-01-02T00:00:01.000000000Z",
    },
    {
      seq: 2,
      role: "assistant",
      parts: [
        { type: "text", content: "I'll search the failed traces from the last 24 hours. " },
        {
          type: "tool_call",
          id: "call_1",
          name: "search_traces",
          arguments: {
            from: "2026-01-01T00:00:00Z",
            to: "2026-01-02T00:00:00Z",
            filter: "status = 'error'",
          },
        },
      ],
      status: "complete",
      turn_trace_id: TRACE_ID,
      created_at: "2026-01-02T00:00:02.000000000Z",
    },
    {
      seq: 3,
      role: "tool",
      parts: [
        {
          type: "tool_result",
          id: "call_1",
          name: "search_traces",
          ok: true,
          result: { rows: scriptedToolRows },
          size_bytes: 812,
          rows: 3,
          stubbed: false,
        },
      ],
      status: "complete",
      turn_trace_id: TRACE_ID,
      created_at: "2026-01-02T00:00:03.000000000Z",
    },
    {
      seq: 4,
      role: "assistant",
      parts: [
        {
          type: "tool_call",
          id: "call_2",
          name: "draft_filter",
          arguments: {
            view: "traces",
            filter: "status = 'error'",
            range: "24h",
            include_eval: false,
            include_experiments: false,
          },
        },
      ],
      status: "complete",
      turn_trace_id: TRACE_ID,
      created_at: "2026-01-02T00:00:04.000000000Z",
    },
    {
      seq: 5,
      role: "tool",
      parts: [
        {
          type: "tool_result",
          id: "call_2",
          name: "draft_filter",
          ok: true,
          result: { status: "drafted" },
          size_bytes: 140,
          stubbed: false,
        },
        {
          type: "draft",
          id: "draft_call_2",
          kind: "filter",
          draft: {
            view: "traces",
            filter: "status = 'error'",
            range: "24h",
            include_eval: false,
            include_experiments: false,
          },
        },
      ],
      status: "complete",
      turn_trace_id: TRACE_ID,
      created_at: "2026-01-02T00:00:05.000000000Z",
    },
    {
      seq: 6,
      role: "assistant",
      parts: [
        {
          type: "text",
          content:
            "Three traces failed in the last 24 hours: 5f1c2a9e, 8b3d7f10 and c41e90aa. Open the filter to see them.",
        },
      ],
      status: "complete",
      turn_trace_id: TRACE_ID,
      created_at: "2026-01-02T00:00:06.000000000Z",
    },
  ];
}

type Fields = Record<string, unknown>;
type StreamEvent = { event: string; data: unknown };
/** A stored message (D-AS-21 stored form) as the detail route returns it. */
type StoredMessage = Fields & { seq: number };

/** A script: the SSE events of a turn, and the messages the server stores for it, from `seq` 1. */
type Script = {
  title: string;
  events: (turnId: number) => StreamEvent[];
  messages: (context: Fields | undefined) => StoredMessage[];
};

/** The time of message `seq`: `base` plus `seq` seconds, with nanosecond digits as the server writes them. */
function stamp(base: string, seq: number): string {
  return new Date(Date.parse(base) + seq * 1000).toISOString().replace("Z", "000000Z");
}

/** `messages` moved to start at `firstSeq`, with their times. */
function placed(messages: StoredMessage[], firstSeq: number, base: string): StoredMessage[] {
  return messages.map((message, index) => ({
    ...message,
    seq: firstSeq + index,
    created_at: stamp(base, firstSeq + index),
  }));
}

/** All the messages of `turns`, one after the other. */
function storedMessages(turns: Ran[], base: string): StoredMessage[] {
  const stored: StoredMessage[] = [];
  for (const turn of turns) {
    stored.push(...placed(turn.script.messages(turn.context), stored.length + 1, base));
  }
  return stored;
}

function isFields(value: unknown): value is Fields {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The question and page context of a turn request; an unreadable body is an empty question. */
async function readTurnRequest(
  request: Request,
): Promise<{ message: string; context: Fields | undefined }> {
  try {
    const body: unknown = await request.json();
    if (!isFields(body)) {
      return { message: "", context: undefined };
    }
    return {
      message: typeof body.message === "string" ? body.message : "",
      context: isFields(body.context) ? body.context : undefined,
    };
  } catch {
    return { message: "", context: undefined };
  }
}

/** Today's script: one scripted turn, whatever the question was. */
const DEFAULT_SCRIPT: Script = {
  title: scriptedTitle,
  events: () => scriptedEvents,
  messages: () => scriptedMessages(),
};

/** The call id of a story turn's one tool call. */
const CALL_ID = "call_1";
const NEW_BASE = "2026-11-19T12:00:00Z";
/** The call id of a story turn's dashboard draft, made after the answer. */
const DASHBOARD_CALL_ID = "call_2";
const DASHBOARD_CLOSING = "Here is the draft. Open it in the editor to review and save it.";

/** What one story turn says and does: its question, one SQL call, the rows it returns and the filter it ends on. */
type TurnSpec = {
  question: string;
  /** The text before the tool call, as two streamed deltas. */
  intro: [string, string];
  call: { name: string; arguments: Fields };
  /** The stored result of the call: the columns and the rows the SQL returns. */
  result: Fields;
  /** The number of rows the call returned. */
  rows: number;
  /** The filter card the turn drafts before its answer. */
  draft: { kind: "filter"; draft: Fields };
  answer: string;
  /** The story's dashboard draft, which the turn drafts after its answer with a second call. */
  dashboard: Fields;
};

const USAGE = { input: 1400, output: 160 };
const COST_USD = 0.0024;

function turnSummaryOf(spec: TurnSpec) {
  return { size_bytes: JSON.stringify(spec.result).length, rows: spec.rows };
}

function dashboardSummaryOf(spec: TurnSpec) {
  return { size_bytes: JSON.stringify(spec.dashboard).length };
}

function storyEvents(spec: TurnSpec, traceId: string, turnId: number): StreamEvent[] {
  return [
    { event: "start", data: { turn_id: turnId, trace_id: traceId } },
    { event: "text", data: { delta: spec.intro[0] } },
    { event: "text", data: { delta: spec.intro[1] } },
    {
      event: "tool_call",
      data: { id: CALL_ID, name: spec.call.name, arguments: spec.call.arguments },
    },
    { event: "tool_result", data: { id: CALL_ID, ok: true, summary: turnSummaryOf(spec) } },
    {
      event: "draft",
      data: { id: `draft_${CALL_ID}`, kind: spec.draft.kind, draft: spec.draft.draft },
    },
    { event: "text", data: { delta: spec.answer } },
    {
      event: "tool_call",
      data: { id: DASHBOARD_CALL_ID, name: "draft_dashboard", arguments: spec.dashboard },
    },
    {
      event: "tool_result",
      data: { id: DASHBOARD_CALL_ID, ok: true, summary: dashboardSummaryOf(spec) },
    },
    {
      event: "draft",
      data: { id: `draft_${DASHBOARD_CALL_ID}`, kind: "dashboard", draft: spec.dashboard },
    },
    { event: "text", data: { delta: DASHBOARD_CLOSING } },
    {
      event: "done",
      data: { finish_reason: "stop", usage: USAGE, cost_usd: COST_USD, steps: 2 },
    },
  ];
}

/** The four stored messages of a story turn: question, call, result with its draft, answer. */
function storyMessages(
  spec: TurnSpec,
  traceId: string,
  context: Fields | undefined,
): StoredMessage[] {
  const reply = { status: "complete", turn_trace_id: traceId, created_at: "" };
  return [
    {
      seq: 1,
      role: "user",
      parts: [{ type: "text", content: spec.question }],
      ...(context === undefined ? {} : { context }),
      status: "complete",
      created_at: "",
    },
    {
      seq: 2,
      role: "assistant",
      parts: [
        { type: "text", content: spec.intro.join("") },
        {
          type: "tool_call",
          id: CALL_ID,
          name: spec.call.name,
          arguments: spec.call.arguments,
        },
      ],
      ...reply,
    },
    {
      seq: 3,
      role: "tool",
      parts: [
        {
          type: "tool_result",
          id: CALL_ID,
          name: spec.call.name,
          ok: true,
          result: spec.result,
          ...turnSummaryOf(spec),
          stubbed: false,
        },
        {
          type: "draft",
          id: `draft_${CALL_ID}`,
          kind: spec.draft.kind,
          draft: spec.draft.draft,
        },
      ],
      ...reply,
    },
    {
      seq: 4,
      role: "assistant",
      parts: [
        { type: "text", content: spec.answer },
        {
          type: "tool_call",
          id: DASHBOARD_CALL_ID,
          name: "draft_dashboard",
          arguments: spec.dashboard,
        },
      ],
      ...reply,
    },
    {
      seq: 5,
      role: "tool",
      parts: [
        {
          type: "tool_result",
          id: DASHBOARD_CALL_ID,
          name: "draft_dashboard",
          ok: true,
          result: { status: "drafted" },
          ...dashboardSummaryOf(spec),
          stubbed: false,
        },
        {
          type: "draft",
          id: `draft_${DASHBOARD_CALL_ID}`,
          kind: "dashboard",
          draft: spec.dashboard,
        },
      ],
      ...reply,
    },
    {
      seq: 6,
      role: "assistant",
      parts: [{ type: "text", content: DASHBOARD_CLOSING }],
      ...reply,
    },
  ];
}

function storyScript(spec: TurnSpec, traceId: string): Script {
  return {
    title: spec.question,
    events: (turnId) => storyEvents(spec, traceId, turnId),
    messages: (context) => storyMessages(spec, traceId, context),
  };
}

/** The id of the generated trace at `index`, as `dataset.ts` writes it. */
function traceIdAt(index: number): string {
  return `a${index.toString(16).padStart(31, "0")}`;
}

/** The SQL call of a story turn, as the assistant's query_sql tool receives it. */
function queryCall(query: string): TurnSpec["call"] {
  return { name: "query_sql", arguments: { query, params: {} } };
}

/** The filter card a story turn ends on: the traces page, with `filter` over `range`. */
function filterDraft(filter: string, range: string): Fields {
  return { view: "traces", filter, range, include_eval: false, include_experiments: false };
}

/** The dashboard draft a story turn ends on: the story dashboard `id`, as the draft card reads it. */
function dashboardDraft(id: string): Fields {
  const dashboard = storyDashboards().find((item) => item.id === id);
  if (dashboard === undefined) {
    throw new Error(`no story dashboard ${id}`);
  }
  return {
    name: dashboard.name,
    description: dashboard.description,
    definition: dashboard.definition,
  };
}

const SUPPORT_SQL =
  "SELECT release, count(*) AS feedback, count(*) FILTER (WHERE score('user_feedback') = 0) AS downvotes, count(*) FILTER (WHERE array_has(tags, 'old-pricing')) AS old_pricing FROM traces WHERE has_score('user_feedback') AND start_time >= now() - INTERVAL '46 hours' GROUP BY release";
const CODING_SQL =
  "SELECT array_has(tags, 'api-retry') AS retried, count(*) AS runs, sum(cost_total) AS cost FROM traces WHERE start_time >= now() - INTERVAL '14 hours' GROUP BY 1";
const VOICE_SQL =
  "SELECT count(*) AS checks, approx_percentile_cont(s.duration_ms, 0.5) AS p50_ms, approx_percentile_cont(s.duration_ms, 0.95) AS p95_ms FROM spans s JOIN traces t ON s.trace_id = t.trace_id WHERE s.name = 'check_availability' AND array_has(t.tags, 'calendar') AND t.start_time >= now() - INTERVAL '30 days'";
const EXTRACTION_SQL =
  "SELECT CASE input_preview WHEN 'invoice-2026-0521.pdf (2 pages)' THEN 'Nordic Freight' WHEN 'invoice-2026-0524.pdf (1 page)' THEN 'Baltic Paper' WHEN 'invoice-2026-0527.pdf (3 pages)' THEN 'Rhine Tools' WHEN 'invoice-2026-0530.pdf (1 page)' THEN 'Lyon Bureau' ELSE 'Other vendors' END AS vendor, count(*) AS invoices, round(avg(CASE WHEN array_has(tags, 're-extracted') THEN 1.0 ELSE 0.0 END), 2) AS re_extracted FROM traces WHERE start_time >= now() - INTERVAL '30 days' GROUP BY 1 ORDER BY 3 DESC LIMIT 4";

/** One story: its hero trace, its stored chat, and its one turn (a question answered from SQL). */
export type Story = {
  story: StoryId;
  conversationId: string;
  /** When the stored chat started; each story is five minutes older than the one before. */
  base: string;
  heroTraceId: string;
  /** The range the stored chat's question carried: the page's range, the same as its filter card's. */
  range: string;
  turns: [TurnSpec];
};

export const STORIES: Story[] = [
  {
    story: "support",
    conversationId: "01900000-0000-7000-8000-000000570801",
    base: "2026-11-19T11:50:00Z",
    heroTraceId: traceIdAt(3),
    range: "7d",
    turns: [
      {
        question: "Why did our thumbs-down rate double since Tuesday?",
        intro: ["I'll count the downvoted answers ", "since Tuesday 14:00, by release. "],
        call: queryCall(SUPPORT_SQL),
        result: {
          columns: ["release", "feedback", "downvotes", "old_pricing"],
          rows: [{ release: "v2.14", feedback: 438, downvotes: 66, old_pricing: 41 }],
        },
        rows: 1,
        draft: {
          kind: "filter",
          draft: filterDraft("release = 'v2.14' and tags @> ['old-pricing']", "7d"),
        },
        dashboard: dashboardDraft(STORY_DASHBOARD_IDS.answerQuality),
        answer:
          "**62%** of the downvoted answers since Tuesday 14:00 quote the old pricing page: retrieval returns `pricing-2025.md`, back in the index since deploy v2.14. That's **41** traces.",
      },
    ],
  },
  {
    story: "coding",
    conversationId: "01900000-0000-7000-8000-000000570802",
    base: "2026-11-19T11:45:00Z",
    heroTraceId: traceIdAt(4),
    range: "24h",
    turns: [
      {
        question: "Why did cost per run triple yesterday?",
        intro: ["I'll compare the runs since the cost rose last night ", "with the day before."],
        call: queryCall(CODING_SQL),
        result: {
          columns: ["retried", "runs", "cost"],
          rows: [
            { retried: false, runs: 359, cost: 189.42 },
            { retried: true, runs: 78, cost: 436.58 },
          ],
        },
        rows: 2,
        draft: { kind: "filter", draft: filterDraft("tags @> ['api-retry']", "24h") },
        dashboard: dashboardDraft(STORY_DASHBOARD_IDS.codeAgentHealth),
        answer:
          "**18%** of runs call `run_tests` more than five times and make **70%** of the cost: they all touch `packages/api`, whose test suite times out at 30 s, so the agent retries.",
      },
    ],
  },
  {
    story: "voice",
    conversationId: "01900000-0000-7000-8000-000000570803",
    base: "2026-11-19T11:40:00Z",
    heroTraceId: traceIdAt(5),
    range: "30d",
    turns: [
      {
        question: "Why do callers hang up?",
        intro: ["I'll look at the turn before each hang-up ", "over the last 30 days."],
        call: queryCall(VOICE_SQL),
        result: {
          columns: ["checks", "p50_ms", "p95_ms"],
          rows: [{ checks: 575, p50_ms: 1486, p95_ms: 3500 }],
        },
        rows: 1,
        draft: {
          kind: "filter",
          draft: filterDraft("tags @> ['calendar']", "30d"),
        },
        dashboard: dashboardDraft(STORY_DASHBOARD_IDS.voiceLatency),
        answer:
          "**71%** of hang-ups follow a turn over 2.5 s, almost all of them `check_availability` on the calendar API (median **1.5 s**, p95 **3.5 s**).",
      },
    ],
  },
  {
    story: "extraction",
    conversationId: "01900000-0000-7000-8000-000000570804",
    base: "2026-11-19T11:35:00Z",
    heroTraceId: traceIdAt(6),
    range: "30d",
    turns: [
      {
        question: "Which vendors' invoices fail the most, and why?",
        intro: ["I'll group the invoices ", "by vendor."],
        call: queryCall(EXTRACTION_SQL),
        result: {
          columns: ["vendor", "invoices", "re_extracted"],
          rows: [
            { vendor: "Nordic Freight", invoices: 95, re_extracted: 0.38 },
            { vendor: "Baltic Paper", invoices: 90, re_extracted: 0.3 },
            { vendor: "Rhine Tools", invoices: 89, re_extracted: 0.29 },
            { vendor: "Lyon Bureau", invoices: 96, re_extracted: 0.29 },
          ],
        },
        rows: 4,
        draft: { kind: "filter", draft: filterDraft("tags @> ['re-extracted']", "30d") },
        dashboard: dashboardDraft(STORY_DASHBOARD_IDS.invoiceExtraction),
        answer:
          'Nordic Freight: **38%** re-extracted, all on `total`. It writes amounts as "1.234,56" and the schema expects a dot; Baltic Paper, Rhine Tools and Lyon Bureau use the same format.',
      },
    ],
  },
];

/** Every story's question, lowercased and trimmed, with its script. */
const SCRIPTS = new Map<string, Script>(
  STORIES.map((story): [string, Script] => {
    const [turn] = story.turns;
    return [turn.question.trim().toLowerCase(), storyScript(turn, story.heroTraceId)];
  }),
);

/** The script that answers `message`: a story's when it is that question (trimmed, any case), else today's. */
export function scriptFor(message: string): Script {
  return SCRIPTS.get(message.trim().toLowerCase()) ?? DEFAULT_SCRIPT;
}

type Summary = {
  id: string;
  title: string;
  message_count: number;
  created_at: string;
  last_active_at: string;
};

/** The detail of a stored story chat: its one turn, as it was run. */
function storyDetail(story: Story) {
  const [turn] = story.turns;
  const messages = placed(
    storyScript(turn, story.heroTraceId).messages({ page: "traces", range: story.range }),
    1,
    story.base,
  );
  return {
    id: story.conversationId,
    title: turn.question,
    message_count: messages.length,
    created_at: stamp(story.base, 0),
    last_active_at: stamp(story.base, messages.length),
    messages,
  };
}

function summaryOf(detail: Summary): Summary {
  const { id, title, message_count, created_at, last_active_at } = detail;
  return { id, title, message_count, created_at, last_active_at };
}

/** The new chat: no messages before its first turn, then every turn run in it. */
function newConversationDetail() {
  const messages = storedMessages(ran, NEW_BASE);
  return {
    ...newConversation,
    title: ran[0]?.script.title ?? "",
    message_count: messages.length,
    last_active_at:
      messages.length === 0 ? newConversation.last_active_at : stamp(NEW_BASE, messages.length),
    messages,
  };
}

/**
 * The conversations of `project` (a decoded API ref): the new chat, then the project's story chat,
 * then the fixture list in the support project only.
 */
function conversationList(project: unknown) {
  const ref = typeof project === "string" ? project : undefined;
  const stories = STORIES.filter((story) => projectOfRef(ref)?.story === story.story).map((story) =>
    summaryOf(storyDetail(story)),
  );
  const fixtures = projectOfRef(ref)?.story === "support" ? assistantConversations.data : [];
  const listed = created ? [summaryOf(newConversationDetail())] : [];
  return { data: [...listed, ...stories, ...fixtures], next_cursor: null };
}

/**
 * Whether conversation `id` is served in `project` (a decoded API ref): a story chat only in its
 * story's project (P-4); every other id, the new chat and the fixtures included, in every project.
 */
function servedIn(id: unknown, project: unknown): boolean {
  const story = STORIES.find((item) => item.conversationId === id);
  const ref = typeof project === "string" ? project : undefined;
  return story === undefined || projectOfRef(ref)?.story === story.story;
}

/**
 * The assistant's write and turn routes for the dev worker. The dev worker installs them before the
 * manifest handlers. The conversation list and a new conversation's detail fall through to the manifest
 * for other ids (a handler that returns nothing lets the next one answer).
 */
export function assistantHandlers(eventDelayMs: number = EVENT_DELAY_MS): HttpHandler[] {
  return [
    http.post(`${PROJECT}/conversations`, () => {
      created = true;
      ran = [];
      return HttpResponse.json(newConversation, { status: 201 });
    }),
    http.get(`${PROJECT}/conversations`, ({ params }) =>
      HttpResponse.json(conversationList(params.project)),
    ),
    http.get(`${PROJECT}/conversations/:id`, ({ params }) => {
      if (params.id === NEW_CONVERSATION_ID) {
        return HttpResponse.json(newConversationDetail());
      }
      if (!servedIn(params.id, params.project)) {
        return undefined;
      }
      const story = STORIES.find((item) => item.conversationId === params.id);
      return story === undefined ? undefined : HttpResponse.json(storyDetail(story));
    }),
    http.patch(`${PROJECT}/conversations/:id`, async ({ params, request }) => {
      if (!servedIn(params.id, params.project)) {
        return undefined;
      }
      const body = (await request.json()) as { title: string };
      return HttpResponse.json({ ...newConversation, id: params.id, title: body.title });
    }),
    http.delete(`${PROJECT}/conversations/:id`, ({ params }) =>
      servedIn(params.id, params.project) ? new HttpResponse(null, { status: 204 }) : undefined,
    ),
    http.put(
      `${PROJECT}/conversations/:id/messages/:seq/feedback`,
      () => new HttpResponse(null, { status: 204 }),
    ),
    http.post(`${PROJECT}/conversations/:id/turns`, async ({ params, request }) => {
      if (!servedIn(params.id, params.project)) {
        return undefined;
      }
      const turn = await readTurnRequest(request);
      const script = scriptFor(turn.message);
      const isNew = params.id === NEW_CONVERSATION_ID;
      const turnId = isNew ? ran.length + 1 : 1;
      if (isNew) {
        ran.push({ script, context: turn.context });
      }
      return turnStream(script.events(turnId), { delayMs: eventDelayMs });
    }),
  ];
}
