import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it } from "vitest";
import { setCurrentConversation, setPanelOpen } from "@/lib/assistant/panel";
import { server } from "@/mocks/node";
import { typeMessage } from "@/test/assistant";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import { turnStream } from "@/test/turn-stream";
import conversation from "../../../../fixtures/api/responses/assistant-conversations/01900000-0000-7000-8000-000000001a01.json";

const DESKTOP_WIDTH = window.innerWidth;
const REF = "acme/support-bot";
const CONVERSATION_ID = "01900000-0000-7000-8000-000000001a01";
const TURNS = "/api/v1/projects/:project/assistant/conversations/:id/turns";
const ITEM = "/api/v1/projects/:project/assistant/conversations/:id";
const EVALUATOR = "/api/v1/projects/:project/evaluators/:id";
const TRACES_URL = "/acme/support-bot/traces";
const SQL_URL = "/acme/support-bot/sql";
const TRACE = "0af7651916cd43dd8448eb211c80319c";
const STAMP = "2026-01-02T00:00:00.000000000Z";
const FILTER = "status = 'error'";
const SQL_QUERY = "SELECT count(*) AS n FROM traces";
const PROMPT = "Rate how helpful {{output}} is.";
const DASHBOARD = "01900000-0000-7000-8000-000000000f01";
const SQL_KEY = "cetra.sql.tabs.acme/support-bot";
const START = { event: "start", data: { turn_id: 5, trace_id: TRACE } };
const DONE = {
  event: "done",
  data: { finish_reason: "stop", usage: { input: 1, output: 1 }, cost_usd: null, steps: 1 },
};

/** The messages the server stores after a turn, added after the fixture's four. */
let stored: unknown[] = [];

beforeEach(() => {
  window.innerWidth = 1280;
  stored = [];
  setCurrentConversation(REF, CONVERSATION_ID);
  server.use(
    http.get(ITEM, () => {
      const base = conversation.messages as unknown[];
      return HttpResponse.json({
        ...conversation,
        message_count: base.length + stored.length,
        messages: [...base, ...stored],
      });
    }),
  );
});

afterEach(() => {
  window.innerWidth = DESKTOP_WIDTH;
  localStorage.clear();
});

/** A turn whose answer holds one draft, streamed and then stored. */
function draftAnswer(kind: string, draft: unknown) {
  const id = "draft_1";
  stored = [
    {
      seq: 5,
      role: "user",
      parts: [{ type: "text", content: "Draft it" }],
      status: "complete",
      created_at: STAMP,
    },
    {
      seq: 6,
      role: "assistant",
      parts: [{ type: "draft", id, kind, draft }],
      status: "complete",
      turn_trace_id: TRACE,
      created_at: STAMP,
    },
  ];
  return turnStream([START, { event: "draft", data: { id, kind, draft } }, DONE]);
}

async function askForDraft(
  user: Awaited<ReturnType<typeof renderApp>>["user"],
  kind: string,
  draft: unknown,
) {
  server.use(http.post(TURNS, () => draftAnswer(kind, draft)));
  await user.click(await screen.findByRole("button", { name: "Assistant" }));
  const region = await screen.findByRole("region", { name: "Assistant" });
  await typeMessage(user, region, "Draft it");
  await user.click(within(region).getByRole("button", { name: "Send" }));
  return region;
}

it("applies_a_filter_draft", async () => {
  const { user, router } = await renderApp(TRACES_URL);
  const region = await askForDraft(user, "filter", {
    view: "traces",
    filter: FILTER,
    range: "24h",
    include_eval: false,
    include_experiments: false,
  });

  await user.click(await within(region).findByRole("button", { name: "Open in traces" }));

  await waitFor(() => {
    expect(router.state.location.pathname).toBe(TRACES_URL);
  });
  expect(router.state.location.search).toMatchObject({ filter: FILTER, range: "24h" });
});

it("sql_draft_opens_a_new_tab_without_touching_the_open_one", async () => {
  localStorage.setItem(
    SQL_KEY,
    JSON.stringify({
      tabs: [{ id: "open", number: 1, draft: { query: "SELECT 1", params: {} } }],
      active: "open",
    }),
  );
  const { user } = await renderApp(SQL_URL);
  const region = await askForDraft(user, "sql", {
    query: SQL_QUERY,
    params: { limit: 10 },
    columns: [],
  });

  // The conversation's earlier "Ran SQL" step has a button with the same name; the draft is last.
  await waitFor(() => {
    expect(within(region).getAllByRole("button", { name: "Open in SQL console" })).toHaveLength(2);
  });
  const buttons = within(region).getAllByRole("button", { name: "Open in SQL console" });
  await user.click(buttons[buttons.length - 1] as HTMLElement);

  await waitFor(() => {
    const saved = JSON.parse(localStorage.getItem(SQL_KEY) ?? "{}") as {
      tabs: { id: string; draft: { query: string; params: Record<string, string> } }[];
      active: string;
    };
    expect(saved.tabs).toHaveLength(2);
    expect(saved.tabs[0]?.draft).toEqual({ query: "SELECT 1", params: {} });
    expect(saved.tabs[1]?.draft).toEqual({ query: SQL_QUERY, params: { limit: "10" } });
    expect(saved.active).toBe(saved.tabs[1]?.id);
  });
});

it("evaluator_draft_prefills_the_form", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators");
  const region = await askForDraft(user, "evaluator", {
    name: "Helpfulness",
    kind: "llm_judge",
    config: { connection_id: "", model: "", prompt: PROMPT, output: { type: "boolean" } },
    output_score: { name: "helpful", data_type: "boolean" },
  });

  await user.click(await within(region).findByRole("button", { name: "Open evaluator form" }));

  expect(await screen.findByDisplayValue("Helpfulness")).toBeInTheDocument();
  expect(screen.getByLabelText("Prompt")).toHaveValue(PROMPT);
  const recorder = recordRequests();
  expect(recorder.calls.filter((call) => call.method !== "GET")).toEqual([]);
  recorder.stop();
  expect(router.state.location.pathname).toBe("/acme/support-bot/evaluators/new");
});

it("rule_draft_with_unknown_evaluator_cannot_apply", async () => {
  server.use(
    http.get(EVALUATOR, () =>
      HttpResponse.json({ status: 404, title: "Not found", code: "not_found" }, { status: 404 }),
    ),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await askForDraft(user, "online_rule", {
    name: "Score failures",
    evaluator_id: "01900000-0000-7000-8000-000000000e01",
    target: "trace",
    variable_mapping: {},
    sample_rate: 1,
    enabled: true,
  });

  expect(await within(region).findByText("Evaluator: Unknown evaluator")).toBeInTheDocument();
  expect(within(region).getByRole("button", { name: "Open rule form" })).toBeDisabled();
});

it("new_dashboard_draft_opens_the_dialog_prefilled", async () => {
  const { user } = await renderApp(TRACES_URL);
  const region = await askForDraft(user, "dashboard", {
    name: "Latency board",
    description: "p95 by model",
    definition: {
      widgets: [
        {
          id: "heading-latency",
          title: "Latency",
          kind: "heading",
          layout: { x: 0, y: 0, w: 60, h: 1 },
        },
      ],
    },
  });

  await user.click(await within(region).findByRole("button", { name: "Open in dashboard editor" }));

  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByLabelText("Name")).toHaveValue("Latency board");
  expect(within(dialog).getByLabelText("Description (optional)")).toHaveValue("p95 by model");
  expect(within(dialog).queryByText("Start from")).not.toBeInTheDocument();
});

it("edit_dashboard_draft_opens_the_editor", async () => {
  const { user } = await renderApp(TRACES_URL);
  const region = await askForDraft(user, "dashboard", {
    dashboard_id: DASHBOARD,
    version: 3,
    name: "Support overview",
    definition: {
      widgets: [
        {
          id: "heading-drafted",
          title: "Drafted heading",
          kind: "heading",
          layout: { x: 0, y: 0, w: 60, h: 1 },
        },
      ],
    },
  });

  await user.click(await within(region).findByRole("button", { name: "Open in dashboard editor" }));

  expect(await screen.findByRole("button", { name: "Save" })).toBeInTheDocument();
  // The draft card in the panel lists the widget too; the editor shows it outside the panel.
  await waitFor(() => {
    expect(screen.getAllByText("Drafted heading").some((el) => !region.contains(el))).toBe(true);
  });
});

it("edit_draft_waits_while_an_edit_is_open", async () => {
  const { user } = await renderApp(`/acme/support-bot/dashboard/${DASHBOARD}`);
  await user.click(await screen.findByRole("button", { name: "Edit" }));
  const region = await askForDraft(user, "dashboard", {
    dashboard_id: DASHBOARD,
    version: 3,
    name: "Support overview",
    definition: {
      widgets: [
        {
          id: "heading-drafted",
          title: "Drafted heading",
          kind: "heading",
          layout: { x: 0, y: 0, w: 60, h: 1 },
        },
      ],
    },
  });

  await user.click(await within(region).findByRole("button", { name: "Open in dashboard editor" }));

  // The open edit keeps its own widgets; the drafted heading is not in its grid.
  expect(screen.getByRole("button", { name: "Save" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Actions for Traffic" })).toBeInTheDocument();
  expect(
    screen.queryAllByText("Drafted heading").filter((el) => !region.contains(el)),
  ).toHaveLength(0);
});

it("apply_closes_the_sheet_on_a_narrow_window", async () => {
  window.innerWidth = 800;
  window.dispatchEvent(new Event("resize"));
  setPanelOpen(true);
  server.use(
    http.post(TURNS, () =>
      draftAnswer("filter", {
        view: "traces",
        filter: FILTER,
        range: "24h",
        include_eval: false,
        include_experiments: false,
      }),
    ),
  );
  const { user, router } = await renderApp(TRACES_URL);
  const region = await screen.findByRole("region", { name: "Assistant" });
  // The sheet shows its skeleton until the status is read, then the composer.
  await within(region).findByText(/Answers use/);
  await typeMessage(user, region, "Draft it");
  await user.click(within(region).getByRole("button", { name: "Send" }));

  await user.click(await within(region).findByRole("button", { name: "Open in traces" }));

  await waitFor(() => {
    expect(screen.queryByRole("region", { name: "Assistant" })).toBeNull();
  });
  expect(router.state.location.pathname).toBe(TRACES_URL);
});

it("evaluator_draft_handed_over_while_the_form_is_open_replaces_it", async () => {
  const { user } = await renderApp("/acme/support-bot/evaluators/new");
  const region = await askForDraft(user, "evaluator", {
    name: "Helpfulness",
    kind: "llm_judge",
    config: { connection_id: "", model: "", prompt: PROMPT, output: { type: "boolean" } },
    output_score: { name: "helpful", data_type: "boolean" },
  });

  await user.click(await within(region).findByRole("button", { name: "Open evaluator form" }));

  expect(await screen.findByDisplayValue("Helpfulness")).toBeInTheDocument();
});
