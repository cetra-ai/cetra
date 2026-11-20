import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { beforeEach, expect, it } from "vitest";
import { emptyDraft, saveDraft } from "@/lib/playground";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import versionTwoFixture from "../../../../fixtures/api/responses/prompts/support-reply/versions/2.json";
import projectFixture from "../../../../fixtures/api/static/project.json";

const CONNECTION_ID = "01900000-0000-7000-8000-000000000400";
const PAGE = "/acme/support-bot/playground";
const PROJECT_KEY = "acme/support-bot";
const TOOL_BLOCK = "Message 2 has tool parts; a prompt version holds text messages only.";

/** An empty draft whose second message (the user's) says `content`. */
function draftSaying(content: string) {
  const draft = emptyDraft(CONNECTION_ID);
  draft.entries[1] = { role: "user", parts: [{ type: "text", content }] };
  return draft;
}

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

beforeEach(() => {
  localStorage.clear();
});

it("saves the messages as a new version of a chat prompt, moving the checked label", async () => {
  saveDraft(PROJECT_KEY, draftSaying("Hello"));
  const requests = recordRequests();
  server.use(
    http.post("/api/v1/projects/:project/prompts/:prompt/versions", () =>
      HttpResponse.json({ ...versionTwoFixture, version: 3 }, { status: 201 }),
    ),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Save as new version…" }));
  const dialog = await screen.findByRole("dialog", { name: "Save as new version" });
  await user.click(within(dialog).getByRole("combobox", { name: "Prompt" }));
  await user.click(await screen.findByRole("option", { name: "support-reply" }));
  await user.click(within(dialog).getByRole("checkbox", { name: "staging" }));
  await user.click(within(dialog).getByRole("button", { name: "Save version" }));

  await waitFor(() =>
    expect(within(dialog).getByRole("status")).toHaveTextContent("Saved as support-reply v3."),
  );
  await waitFor(() =>
    expect(
      requests.calls.find((call) => call.method === "POST" && call.path.endsWith("/versions"))
        ?.body,
    ).toEqual({
      template: [
        { role: "system", content: "" },
        { role: "user", content: "Hello" },
      ],
      config: { model: "gpt-5-mini", connection_id: CONNECTION_ID },
      labels: ["staging"],
    }),
  );
  requests.stop();
});

it("blocks the save and names the message when it holds tool parts", async () => {
  const draft = draftSaying("Hello");
  draft.entries[1] = {
    role: "assistant",
    parts: [{ type: "tool_call", id: "call_1", name: "lookup", arguments: {} }],
  };
  saveDraft(PROJECT_KEY, draft);
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Save as new version…" }));
  const dialog = await screen.findByRole("dialog", { name: "Save as new version" });

  expect(within(dialog).getAllByText(TOOL_BLOCK).length).toBeGreaterThan(0);
  expect(within(dialog).getByRole("button", { name: "Save version" })).toBeDisabled();
});

it("a member cannot tick the protected production label", async () => {
  server.use(projectAs("member"));
  saveDraft(PROJECT_KEY, draftSaying("Hello"));
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Save as new version…" }));
  const dialog = await screen.findByRole("dialog", { name: "Save as new version" });
  await user.click(within(dialog).getByRole("combobox", { name: "Prompt" }));
  await user.click(await screen.findByRole("option", { name: "support-reply" }));

  expect(within(dialog).getByRole("checkbox", { name: "production" })).toBeDisabled();
  expect(within(dialog).getByRole("checkbox", { name: "staging" })).toBeEnabled();
});

it("saves the messages as a new chat prompt with the production label", async () => {
  server.use(projectAs("owner"));
  saveDraft(PROJECT_KEY, draftSaying("Hello"));
  const requests = recordRequests();
  server.use(
    http.post("/api/v1/projects/:project/prompts", () =>
      HttpResponse.json({ ...versionTwoFixture, name: "faq-bot", version: 1 }, { status: 201 }),
    ),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Save as new version…" }));
  const dialog = await screen.findByRole("dialog", { name: "Save as new version" });
  await user.click(within(dialog).getByRole("radio", { name: "New prompt" }));
  await user.type(within(dialog).getByLabelText("Name"), "faq-bot");
  await user.click(within(dialog).getByRole("button", { name: "Save version" }));

  await waitFor(() =>
    expect(within(dialog).getByRole("status")).toHaveTextContent("Saved as faq-bot v1."),
  );
  await waitFor(() =>
    expect(
      requests.calls.find((call) => call.method === "POST" && call.path.endsWith("/prompts"))?.body,
    ).toEqual({
      name: "faq-bot",
      type: "chat",
      template: [
        { role: "system", content: "" },
        { role: "user", content: "Hello" },
      ],
      config: { model: "gpt-5-mini", connection_id: CONNECTION_ID },
      labels: ["production"],
    }),
  );
  requests.stop();
});
