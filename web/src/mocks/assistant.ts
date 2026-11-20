import { type HttpHandler, HttpResponse, http } from "msw";
import assistantConversations from "../../../fixtures/api/responses/assistant-conversations.json";
import { turnStream } from "../test/turn-stream";

const PROJECT = "/api/v1/projects/:project/assistant";
const NEW_CONVERSATION_ID = "01900000-0000-7000-8000-000000001a02";
const TRACE_ID = "0af7651916cd43dd8448eb211c80319c";
const EVENT_DELAY_MS = 120;

/** One scripted turn per project, held in module state; `resetAssistantMock()` clears it. */
let created = false;
let turnRan = false;

export function resetAssistantMock(): void {
  created = false;
  turnRan = false;
}

const newConversation = {
  id: NEW_CONVERSATION_ID,
  title: "",
  message_count: 0,
  created_at: "2026-01-02T00:00:00.000000000Z",
  last_active_at: "2026-01-02T00:00:00.000000000Z",
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

function conversationDetail() {
  return {
    ...newConversation,
    title: scriptedTitle,
    message_count: 6,
    last_active_at: "2026-01-02T00:00:06.000000000Z",
    messages: scriptedMessages(),
  };
}

/** The new chat in the list first, then the fixture list. */
function conversationList() {
  const fixtures = assistantConversations.data;
  if (!created) {
    return { data: fixtures, next_cursor: null };
  }
  const listed = turnRan
    ? {
        ...newConversation,
        title: scriptedTitle,
        message_count: 6,
        last_active_at: "2026-01-02T00:00:06.000000000Z",
      }
    : newConversation;
  return { data: [listed, ...fixtures], next_cursor: null };
}

/**
 * The assistant's write and turn routes for the dev worker. The dev worker installs them before the
 * manifest handlers. The conversation list and a new conversation's detail fall through to the manifest
 * for other ids (a handler that returns nothing lets the next one answer).
 */
export function assistantHandlers(): HttpHandler[] {
  return [
    http.post(`${PROJECT}/conversations`, () => {
      created = true;
      return HttpResponse.json(newConversation, { status: 201 });
    }),
    http.get(`${PROJECT}/conversations`, () => HttpResponse.json(conversationList())),
    http.get(`${PROJECT}/conversations/:id`, ({ params }) => {
      if (params.id !== NEW_CONVERSATION_ID) {
        return undefined;
      }
      return HttpResponse.json(
        turnRan ? conversationDetail() : { ...newConversation, messages: [] },
      );
    }),
    http.patch(`${PROJECT}/conversations/:id`, async ({ params, request }) => {
      const body = (await request.json()) as { title: string };
      return HttpResponse.json({ ...newConversation, id: params.id, title: body.title });
    }),
    http.delete(`${PROJECT}/conversations/:id`, () => new HttpResponse(null, { status: 204 })),
    http.put(
      `${PROJECT}/conversations/:id/messages/:seq/feedback`,
      () => new HttpResponse(null, { status: 204 }),
    ),
    http.post(`${PROJECT}/conversations/:id/turns`, () => {
      turnRan = true;
      return turnStream(scriptedEvents, { delayMs: EVENT_DELAY_MS });
    }),
  ];
}
