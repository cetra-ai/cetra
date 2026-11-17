import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { ADA_ID, PROJECT_ID } from "@/test/fixtures";
import { renderApp } from "@/test/render";
import { serveAnnotationDefinitions } from "@/test/score-definitions";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const TRACE = "a1000000000000000000000000000001";
const SECOND = "a1000000000000000000000000000002";

/** The caller's saved `resolved: Yes` on the session, so the sheet prefills it. */
const OWN_RESOLVED: Schema<"Score"> = {
  project_id: PROJECT_ID,
  score_id: "01900000-0000-7000-8000-000000000330",
  target: { type: "session", session_id: "sess-001" },
  session_id: "sess-001",
  user_id: "user-42",
  name: "resolved",
  value: { data_type: "boolean", value: true },
  source: "human",
  author_user_id: ADA_ID,
  created_at: "2026-01-01T10:00:30.000000000Z",
  updated_at: "2026-01-01T10:00:30.000000000Z",
  deleted: false,
  ingest_time: "2026-01-01T10:00:30.000000000Z",
};

it("selects a turn when its bubbles are clicked, not only its meta line", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sessions/sess-001");
  const conversation = await screen.findByRole("region", { name: "Conversation" });
  await user.click(within(conversation).getByText("Can I change the delivery address?"));
  expect(router.state.location.search).toMatchObject({ trace: SECOND });
  expect(await screen.findByRole("region", { name: "Trace" })).toBeInTheDocument();
});

it("renders sess-001 turns in order and opens the selected trace", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sessions/sess-001");
  const conversation = await screen.findByRole("region", { name: "Conversation" });
  const first = within(conversation).getByText("My order 1234 hasn't arrived.");
  const second = within(conversation).getByText("Can I change the delivery address?");
  expect(first.compareDocumentPosition(second) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(
    within(conversation).getByText("Your order 1234 shipped and arrives tomorrow."),
  ).toBeInTheDocument();
  expect(screen.getByText("2 turns")).toBeInTheDocument();
  expect(screen.getAllByText(scoreBadge("resolved ✓")).length).toBeGreaterThan(0);
  expect(screen.queryByRole("region", { name: "Trace" })).not.toBeInTheDocument();

  const turns = within(conversation).getAllByRole("button", { name: /support-agent/ });
  expect(turns).toHaveLength(2);
  const secondTurn = turns[1];
  if (!secondTurn) {
    throw new Error("missing second turn");
  }
  await user.click(secondTurn);
  expect(router.state.location.search).toMatchObject({ trace: SECOND });
  expect(await screen.findByRole("region", { name: "Trace" })).toBeInTheDocument();
});

it("returns from a picked transcript block to the trace summary", async () => {
  const { user } = await renderApp(`/acme/support-bot/sessions/sess-001?trace=${TRACE}`);
  await user.click(await screen.findByRole("radio", { name: "Transcript" }));
  const transcript = await screen.findByRole("region", { name: "Transcript" });
  expect(await screen.findByRole("region", { name: "Trace summary" })).toBeInTheDocument();

  await user.click(within(transcript).getByRole("button", { name: /get_order_status/ }));
  expect(await screen.findByRole("region", { name: "Span" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Trace summary" })).not.toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Trace summary" }));
  expect(await screen.findByRole("region", { name: "Trace summary" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Span" })).not.toBeInTheDocument();
});

it("follows the session link from a trace header", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/traces?trace=${TRACE}`);
  const header = await screen.findByRole("region", { name: "Trace" });
  await user.click(within(header).getByRole("link", { name: "sess-001" }));
  expect(router.state.location.pathname).toBe("/acme/support-bot/sessions/sess-001");
  expect(await screen.findByRole("region", { name: "Conversation" })).toBeInTheDocument();
});

it("annotates the session from the side sheet", async () => {
  serveAnnotationDefinitions();
  const bodies: unknown[] = [];
  server.use(
    http.get("/api/v1/projects/:project/scores", () =>
      HttpResponse.json({ data: [OWN_RESOLVED], next_cursor: null }),
    ),
    http.post("/api/v1/projects/:project/scores", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json({ score_ids: ["01900000-0000-7000-8000-000000000320"] });
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sessions/sess-001");
  await screen.findByRole("region", { name: "Conversation" });
  await user.click(screen.getByRole("button", { name: "Annotate" }));

  const sheet = await screen.findByRole("dialog", { name: "Annotate session" });
  const resolved = await within(sheet).findByRole("radiogroup", { name: "resolved" });
  expect(within(resolved).getByRole("radio", { name: "Yes" })).toBeChecked();
  await user.click(within(resolved).getByRole("radio", { name: "No" }));
  await user.click(within(sheet).getByRole("button", { name: "Save" }));

  await within(sheet).findByText("Saved.");
  expect(bodies).toEqual([
    {
      scores: [
        {
          target: { type: "session", session_id: "sess-001" },
          user_id: "user-42",
          name: "resolved",
          value: { data_type: "boolean", value: false },
        },
      ],
    },
  ]);
});

it("does not let a viewer annotate the session", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp("/acme/support-bot/sessions/sess-001");
  await screen.findByRole("region", { name: "Conversation" });
  const annotate = screen.getByRole("button", { name: "Annotate" });
  expect(annotate).toBeDisabled();
  // The disabled button sits in a focusable wrapper that carries the reason.
  expect(annotate.parentElement).toHaveAccessibleDescription("Only members can annotate");
});

const NOW = "2026-01-01T10:00:00.000000000Z";
const OK_TRACE = "b1000000000000000000000000000001";
const FAILED_TRACE = "b1000000000000000000000000000002";
const LONG_TRACE = "b1000000000000000000000000000003";

function traceSummary(
  traceId: string,
  name: string,
  overrides: Partial<Schema<"TraceSummary">> = {},
): Schema<"TraceSummary"> {
  return {
    trace_id: traceId,
    name,
    start_time: NOW,
    end_time: NOW,
    duration_ns: 2_000_000_000,
    error_count: 0,
    has_root: true,
    llm_count: 1,
    span_count: 2,
    metadata: {},
    models: [],
    scores: [],
    status: "ok",
    tags: [],
    ...overrides,
  };
}

const ERROR_SESSION: Schema<"SessionSummary"> = {
  session_id: "sess-errors",
  start_time: NOW,
  end_time: NOW,
  duration_ns: 6_000_000_000,
  trace_count: 3,
  error_count: 1,
  error_trace_count: 0,
  scores: [
    {
      name: "resolved",
      score_id: "01900000-0000-7000-8000-000000000340",
      source: "human",
      value: { data_type: "boolean", value: true },
    },
  ],
  tags: ["billing"],
  user_id: "user-42",
  total_tokens: 1234,
  cost_total: 0.0123,
};

const ERROR_SESSION_TURNS: Schema<"SessionTurn">[] = [
  {
    trace: traceSummary(OK_TRACE, "lookup-order", { cost_total: 0.002, tags: [] }),
    input: {
      format: "messages",
      value: [{ role: "user", parts: [{ type: "text", content: "Where is my order?" }] }],
    },
    output: {
      format: "messages",
      value: [
        {
          role: "assistant",
          parts: [
            {
              type: "text",
              content: "Try these steps:\n\n- check the tracking page\n- wait a day",
            },
            { type: "tool_call", id: "call-1", name: "get_order_status" },
          ],
        },
      ],
    },
  },
  {
    trace: traceSummary(FAILED_TRACE, "refund-flow", { status: "error", error_count: 1 }),
    input: { format: "text", value: "Refund order 1234" },
    output: { format: "json", value: { error: "payment timeout" } },
  },
  {
    trace: traceSummary(LONG_TRACE, "summary-flow"),
    input: { format: "text", value: "x".repeat(20_001) },
    output: undefined,
  },
];

/**
 * Matches the element whose whole text is `text`. A score badge keeps its name and its value
 * in separate elements, so a plain text match does not find `name value` as one string.
 */
function scoreBadge(text: string) {
  return (_content: string, element: Element | null) =>
    element?.textContent === text && element.children.length > 0;
}

function serveErrorSession() {
  server.use(
    http.get("/api/v1/projects/:project/sessions/:sessionId", () =>
      HttpResponse.json({
        session: ERROR_SESSION,
        turns: ERROR_SESSION_TURNS,
        truncated: false,
      }),
    ),
  );
}

it("shows the session header and selects the first failed turn from the errors chip", async () => {
  serveErrorSession();
  const { router, user } = await renderApp("/acme/support-bot/sessions/sess-errors");
  const conversation = await screen.findByRole("region", { name: "Conversation" });
  expect(screen.getByText("sess-errors")).toBeInTheDocument();
  expect(screen.getByText("3 turns")).toBeInTheDocument();
  expect(screen.getByText("billing")).toBeInTheDocument();
  expect(screen.getByText(scoreBadge("resolved ✓"))).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "1 errors" }));
  expect(router.state.location.search).toMatchObject({ trace: FAILED_TRACE });
  expect(within(conversation).getByRole("button", { name: /refund-flow/ })).toHaveAttribute(
    "aria-current",
    "true",
  );
});

it("shows a turn as chat bubbles, and the payload switch only in details", async () => {
  serveErrorSession();
  const { user } = await renderApp("/acme/support-bot/sessions/sess-errors");
  const conversation = await screen.findByRole("region", { name: "Conversation" });

  expect(within(conversation).getByText("Where is my order?")).toBeInTheDocument();
  const items = within(conversation).getAllByRole("listitem");
  expect(items.map((item) => item.textContent)).toEqual(["check the tracking page", "wait a day"]);
  expect(within(conversation).getByText("1 tool calls")).toBeInTheDocument();
  expect(within(conversation).getByText('{"error":"payment timeout"}')).toBeInTheDocument();
  expect(within(conversation).queryByText("Pretty")).not.toBeInTheDocument();

  const actions = within(conversation).getAllByRole("button", { name: "Turn actions" });
  const failedActions = actions[1];
  if (!failedActions) {
    throw new Error("missing second turn actions");
  }
  await user.click(failedActions);
  failedActions.focus();
  await user.keyboard("{Enter}");
  await user.click(await screen.findByRole("menuitem", { name: "Show details" }));
  expect((await within(conversation).findAllByText("Pretty")).length).toBeGreaterThan(0);

  failedActions.focus();
  await user.keyboard("{Enter}");
  await user.click(await screen.findByRole("menuitem", { name: "Hide details" }));
  expect(within(conversation).queryByText("Pretty")).not.toBeInTheDocument();
  expect(within(conversation).getByText('{"error":"payment timeout"}')).toBeInTheDocument();
});

it("marks a failed turn and caps a long bubble", async () => {
  serveErrorSession();
  await renderApp("/acme/support-bot/sessions/sess-errors");
  const conversation = await screen.findByRole("region", { name: "Conversation" });

  const failed = within(conversation).getByRole("button", { name: /refund-flow/ });
  expect(within(failed).getByText("Failed")).toBeInTheDocument();
  const ok = within(conversation).getByRole("button", { name: /lookup-order/ });
  expect(within(ok).queryByText("Failed")).not.toBeInTheDocument();

  expect(within(conversation).getByText("Truncated, see details")).toBeInTheDocument();
});

it("goes back to the sessions list from the header", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sessions/sess-001");
  await user.click(await screen.findByRole("link", { name: "Back to sessions" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions");
  });
});

it("goes back to the trace the session was opened from", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/traces?trace=${TRACE}`);
  const header = await screen.findByRole("region", { name: "Trace" });
  await user.click(within(header).getByRole("link", { name: "sess-001" }));
  await user.click(await screen.findByRole("link", { name: "Back to Trace" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/traces");
    expect(router.state.location.search.trace).toBe(TRACE);
  });
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
  });
});
