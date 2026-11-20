import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { STORAGE_PREFIX } from "@/product";
import { openMenu } from "@/test/assistant";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import conversations from "../../../../fixtures/api/responses/assistant-conversations.json";
import meDocument from "../../../../fixtures/api/static/me.json";
import projectDocument from "../../../../fixtures/api/static/project.json";

const DESKTOP_WIDTH = window.innerWidth;
const PROJECT = "/api/v1/projects/:project";
const STATUS = "/api/v1/projects/:project/assistant";
const LIST = "/api/v1/projects/:project/assistant/conversations";
const ITEM = "/api/v1/projects/:project/assistant/conversations/:id";
const REF = "acme/support-bot";
const CURRENT_KEY = `${STORAGE_PREFIX}assistant.conversation.${REF}`;
const EXISTING_ID = "01900000-0000-7000-8000-000000001a01";
const NEW_ID = "01900000-0000-7000-8000-000000001a02";
const NEW_CHAT = {
  id: NEW_ID,
  title: "",
  message_count: 0,
  created_at: "2026-01-02T00:00:00.000000000Z",
  last_active_at: "2026-01-02T00:00:00.000000000Z",
};

beforeEach(() => {
  window.innerWidth = 1280;
});

afterEach(() => {
  window.innerWidth = DESKTOP_WIDTH;
  localStorage.clear();
});

/** The panel is the page's region named Assistant. */
function panel(): HTMLElement | null {
  return screen.queryByRole("region", { name: "Assistant" });
}

async function openPanelFromTopBar(user: Awaited<ReturnType<typeof renderApp>>["user"]) {
  await user.click(await screen.findByRole("button", { name: "Assistant" }));
  return screen.findByRole("region", { name: "Assistant" });
}

it("opens the panel from the top bar and shows the connection notice", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");

  const region = await openPanelFromTopBar(user);
  expect(await within(region).findByText(/Answers use/)).toHaveTextContent("OpenAI");
  expect(within(region).getByText(/Answers use/)).toHaveTextContent("gpt-4o");
  expect(within(region).getByText(/sent to that provider/)).toBeInTheDocument();
});

it("keeps the typed text on an evaluator form when the panel opens and closes", async () => {
  const { user } = await renderApp("/acme/support-bot/evaluators/new");

  const name = (await screen.findAllByLabelText("Name"))[0] as HTMLElement;
  await user.type(name, "Latency judge");
  const region = await openPanelFromTopBar(user);
  await user.click(within(region).getByRole("button", { name: "Close the assistant" }));

  await waitFor(() => {
    expect(panel()).not.toBeInTheDocument();
  });
  expect(name).toHaveValue("Latency judge");
});

it("toggles the panel with Ctrl+I", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("heading", { level: 1, name: "Traces" });

  await user.keyboard("{Control>}i{/Control}");
  expect(await screen.findByRole("region", { name: "Assistant" })).toBeInTheDocument();
  await user.keyboard("{Control>}i{/Control}");
  await waitFor(() => {
    expect(panel()).not.toBeInTheDocument();
  });
});

it("keeps the panel open when the user goes to a project settings page", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await openPanelFromTopBar(user);

  await user.click(await screen.findByRole("link", { name: "Settings" }));

  await waitFor(() => {
    expect(screen.queryByRole("heading", { level: 1, name: "Traces" })).not.toBeInTheDocument();
  });
  expect(panel()).toBeInTheDocument();
});

it("hides the button from viewers", async () => {
  server.use(http.get(PROJECT, () => HttpResponse.json({ ...projectDocument, role: "viewer" })));
  await renderApp("/acme/support-bot/traces");

  await screen.findByRole("heading", { level: 1, name: "Traces" });
  expect(screen.queryByRole("button", { name: "Assistant" })).not.toBeInTheDocument();
});

it("says the assistant is off in the organization, with a link for admins", async () => {
  server.use(http.get(STATUS, () => HttpResponse.json({ usable: false, reason: "disabled" })));
  const { user } = await renderApp("/acme/support-bot/traces");

  const region = await openPanelFromTopBar(user);
  expect(
    await within(region).findByText("The assistant is off in this organization."),
  ).toBeInTheDocument();
  expect(within(region).getByRole("link", { name: "Turn it on" })).toHaveAttribute(
    "href",
    "/acme/settings/assistant",
  );
});

it("hides the admin link from members", async () => {
  server.use(
    http.get(STATUS, () => HttpResponse.json({ usable: false, reason: "disabled" })),
    http.get("/api/auth/me", () =>
      HttpResponse.json({
        ...meDocument,
        orgs: meDocument.orgs.map((org) => ({ ...org, role: "member" })),
      }),
    ),
  );
  const { user } = await renderApp("/acme/support-bot/traces");

  const region = await openPanelFromTopBar(user);
  expect(
    await within(region).findByText("The assistant is off in this organization."),
  ).toBeInTheDocument();
  expect(within(region).queryByRole("link", { name: "Turn it on" })).not.toBeInTheDocument();
});

it("explains a deleted connection to org admins", async () => {
  server.use(http.get(STATUS, () => HttpResponse.json({ usable: false, reason: "no_connection" })));
  const { user } = await renderApp("/acme/support-bot/traces");

  const region = await openPanelFromTopBar(user);
  expect(
    await within(region).findByText("The assistant's LLM connection was deleted."),
  ).toBeInTheDocument();
  expect(within(region).getByRole("link", { name: "Turn it on" })).toBeInTheDocument();
});

it("explains the restore mode and offers no link", async () => {
  server.use(http.get(STATUS, () => HttpResponse.json({ usable: false, reason: "restore_mode" })));
  const { user } = await renderApp("/acme/support-bot/traces");

  const region = await openPanelFromTopBar(user);
  expect(
    await within(region).findByText(
      "The assistant is paused while this server is in restore mode.",
    ),
  ).toBeInTheDocument();
  expect(within(region).queryByRole("link")).not.toBeInTheDocument();
});

it("explains that the assistant is for members when the status is forbidden", async () => {
  server.use(http.get(STATUS, () => HttpResponse.json({ usable: false, reason: "forbidden" })));
  const { user } = await renderApp("/acme/support-bot/traces");

  const region = await openPanelFromTopBar(user);
  expect(
    await within(region).findByText(
      "The assistant is for members, admins and owners of this project.",
    ),
  ).toBeInTheDocument();
});

it("creates a chat and selects it", async () => {
  let created = false;
  server.use(
    http.post(LIST, () => {
      created = true;
      return HttpResponse.json(NEW_CHAT, { status: 201 });
    }),
    http.get(LIST, () =>
      HttpResponse.json({
        data: created ? [NEW_CHAT, ...conversations.data] : conversations.data,
        next_cursor: null,
      }),
    ),
  );
  const requests = recordRequests();
  const { user } = await renderApp("/acme/support-bot/traces");
  const region = await openPanelFromTopBar(user);

  await user.click(within(region).getByRole("button", { name: "New chat" }));

  await waitFor(() => {
    expect(localStorage.getItem(CURRENT_KEY)).toBe(NEW_ID);
  });
  requests.stop();
  // The traces page sends its own POSTs (its charts); only the assistant's are counted.
  expect(
    requests.calls
      .filter((call) => call.method === "POST" && call.path.includes("/assistant/"))
      .map((call) => call.path),
  ).toEqual([`/api/v1/projects/${encodeURIComponent(REF)}/assistant/conversations`]);
});

it("renames and deletes the selected chat", async () => {
  localStorage.setItem(CURRENT_KEY, EXISTING_ID);
  let title = conversations.data[0]?.title ?? "";
  server.use(
    http.get(LIST, () =>
      HttpResponse.json({
        data: [{ ...conversations.data[0], title }],
        next_cursor: null,
      }),
    ),
    http.patch(ITEM, async ({ request }) => {
      const body = (await request.json()) as { title: string };
      title = body.title;
      return HttpResponse.json({ ...conversations.data[0], title });
    }),
    http.delete(ITEM, () => new HttpResponse(null, { status: 204 })),
  );
  const requests = recordRequests();
  const { user } = await renderApp("/acme/support-bot/traces");
  const region = await openPanelFromTopBar(user);

  await openMenu(
    user,
    await within(region).findByRole("button", { name: "Why did checkout errors spike?" }),
  );
  await user.click(await screen.findByRole("menuitem", { name: "Rename" }));
  const titleField = await screen.findByRole("textbox", { name: "Title" });
  await user.clear(titleField);
  await user.type(titleField, "Checkout errors");
  await user.click(screen.getByRole("button", { name: "Rename" }));

  await waitFor(() => {
    expect(requests.calls.find((call) => call.method === "PATCH")?.body).toEqual({
      title: "Checkout errors",
    });
  });

  await openMenu(user, await within(region).findByRole("button", { name: "Checkout errors" }));
  await user.click(await screen.findByRole("menuitem", { name: "Delete" }));
  await user.click(await screen.findByRole("button", { name: "Delete" }));

  await waitFor(() => {
    expect(localStorage.getItem(CURRENT_KEY)).toBeNull();
  });
  requests.stop();
  expect(requests.calls.filter((call) => call.method === "DELETE")).toEqual([
    {
      method: "DELETE",
      path: `/api/v1/projects/${encodeURIComponent(REF)}/assistant/conversations/${EXISTING_ID}`,
    },
  ]);
});
