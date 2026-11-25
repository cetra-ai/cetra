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

/** What one story turn says and does. */
type TurnSpec = {
  question: string;
  /** The text before the tool call, as two streamed deltas. */
  intro: [string, string];
  call: { name: string; arguments: Fields };
  /** The stored result of the call. */
  result: Fields;
  /** The rows of a search; absent for a draft. */
  rows?: number;
  draft?: { kind: "dashboard"; draft: Fields };
  answer: string;
};

const USAGE = { input: 1400, output: 160 };
const COST_USD = 0.0024;

function turnSummaryOf(spec: TurnSpec) {
  const size_bytes = JSON.stringify(spec.result).length;
  return spec.rows === undefined ? { size_bytes } : { size_bytes, rows: spec.rows };
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
    ...(spec.draft === undefined
      ? []
      : [
          {
            event: "draft",
            data: { id: `draft_${CALL_ID}`, kind: spec.draft.kind, draft: spec.draft.draft },
          },
        ]),
    { event: "text", data: { delta: spec.answer } },
    {
      event: "done",
      data: { finish_reason: "stop", usage: USAGE, cost_usd: COST_USD, steps: 1 },
    },
  ];
}

/** The four stored messages of a story turn: question, call, result (and draft), answer. */
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
        ...(spec.draft === undefined
          ? []
          : [
              {
                type: "draft",
                id: `draft_${CALL_ID}`,
                kind: spec.draft.kind,
                draft: spec.draft.draft,
              },
            ]),
      ],
      ...reply,
    },
    { seq: 4, role: "assistant", parts: [{ type: "text", content: spec.answer }], ...reply },
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

/** A search result: one row per trace. */
function searchResult(indexes: number[]): { result: Fields; rows: number } {
  return {
    result: { rows: indexes.map((index) => ({ trace_id: traceIdAt(index) })) },
    rows: indexes.length,
  };
}

const STORY_CONTEXT = { page: "traces", range: "24h" };

/** The turn-2 draft of a story: the story dashboard `id` (`story-dashboards.ts`), as the draft card shows it. */
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

/** One story: its hero trace, its stored chat, and its two turns (an answer, then a dashboard draft). */
export type Story = {
  story: StoryId;
  conversationId: string;
  /** When the stored chat started; each story is five minutes older than the one before. */
  base: string;
  heroTraceId: string;
  turns: [TurnSpec, TurnSpec];
};

function searchTurn(
  question: string,
  intro: [string, string],
  filter: string,
  from: string,
  found: { result: Fields; rows: number },
  answer: string,
): TurnSpec {
  return {
    question,
    intro,
    call: { name: "search_traces", arguments: { from, to: "2026-11-19T12:00:00Z", filter } },
    result: found.result,
    rows: found.rows,
    answer,
  };
}

function dashboardTurn(question: string, draft: Fields): TurnSpec {
  return {
    question,
    intro: ["I'll draft a dashboard ", "with four widgets. "],
    call: { name: "draft_dashboard", arguments: draft },
    result: { status: "drafted" },
    draft: { kind: "dashboard", draft },
    answer: "Here is the draft. Open it in the editor to review and save it.",
  };
}

export const STORIES: Story[] = [
  {
    story: "support",
    conversationId: "01900000-0000-7000-8000-000000570801",
    base: "2026-11-19T11:50:00Z",
    heroTraceId: traceIdAt(3),
    turns: [
      searchTurn(
        "Which answers scored low today?",
        ["I'll look at today's support answers ", "and their scores. "],
        "tags @> ['email']",
        "2026-11-19T00:00:00Z",
        searchResult([11, 18, 23, 31, 38, 44]),
        "Answers about refunds scored lowest today. The CSV export answer (support-agent) scored 0.9.",
      ),
      dashboardTurn(
        "Build me a dashboard of answer quality.",
        dashboardDraft(STORY_DASHBOARD_IDS.answerQuality),
      ),
    ],
  },
  {
    story: "coding",
    conversationId: "01900000-0000-7000-8000-000000570802",
    base: "2026-11-19T11:45:00Z",
    heroTraceId: traceIdAt(4),
    turns: [
      searchTurn(
        "Which traces failed in the last day?",
        ["I'll search the failed traces ", "from the last day. "],
        "status = 'error'",
        "2026-11-18T12:00:00Z",
        searchResult([4]),
        "One: coding-agent (a0000000…0004). Its first run_tests call failed with 1 failed, 47 passed, and the second run passed.",
      ),
      dashboardTurn(
        "Build me a dashboard for code-agent: failed runs, cost per run and p95 duration.",
        dashboardDraft(STORY_DASHBOARD_IDS.codeAgentHealth),
      ),
    ],
  },
  {
    story: "voice",
    conversationId: "01900000-0000-7000-8000-000000570803",
    base: "2026-11-19T11:40:00Z",
    heroTraceId: traceIdAt(5),
    turns: [
      searchTurn(
        "Which calls were slow today?",
        ["I'll look for slow calls ", "from today. "],
        "tags @> ['voice'] and duration_ns > 3000000000",
        "2026-11-19T00:00:00Z",
        searchResult([5, 9, 13, 16, 21, 25, 28, 33, 37, 40, 43, 47]),
        "Several calls took over 3 s. In voice-agent (a0000000…0005), check_availability alone took 1.22 s.",
      ),
      dashboardTurn(
        "Build me a dashboard of voice latency.",
        dashboardDraft(STORY_DASHBOARD_IDS.voiceLatency),
      ),
    ],
  },
  {
    story: "extraction",
    conversationId: "01900000-0000-7000-8000-000000570804",
    base: "2026-11-19T11:35:00Z",
    heroTraceId: traceIdAt(6),
    turns: [
      searchTurn(
        "Which invoices needed a retry?",
        ["I'll look for invoices ", "that ran more than once. "],
        "tags @> ['billing']",
        "2026-11-18T12:00:00Z",
        searchResult([6, 14, 29]),
        "In extract-invoice (a0000000…0006), the first model read the total as 1,248.00 instead of 1,284.00, and the retry fixed it.",
      ),
      dashboardTurn(
        "Build me a dashboard of invoices, schema-check failures and cost.",
        dashboardDraft(STORY_DASHBOARD_IDS.invoiceExtraction),
      ),
    ],
  },
];

/** Every story turn's question, lowercased and trimmed, with its script. */
const SCRIPTS = new Map<string, Script>(
  STORIES.flatMap((story) =>
    story.turns.map((turn): [string, Script] => [
      turn.question.trim().toLowerCase(),
      storyScript(turn, story.heroTraceId),
    ]),
  ),
);

/** The script that answers `message`: a story turn's when it is that question (trimmed, any case), else today's. */
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

/** The detail of a stored story chat: both turns, as they were run. */
function storyDetail(story: Story) {
  const turns: Ran[] = story.turns.map((turn) => ({
    script: storyScript(turn, story.heroTraceId),
    context: STORY_CONTEXT,
  }));
  const messages = storedMessages(turns, story.base);
  return {
    id: story.conversationId,
    title: story.turns[0].question,
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
