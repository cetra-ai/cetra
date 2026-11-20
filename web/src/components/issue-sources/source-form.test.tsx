import { screen, waitFor } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import connectionsFixture from "../../../../fixtures/api/static/llm-connections.json";
import sourcesFixture from "../../../../fixtures/api/static/issue-sources.json";

const NEW_URL = "/acme/support-bot/issue-sources/new";
const TOPIC = sourcesFixture.data[1];
const EDIT_URL = `/acme/support-bot/issue-sources/${TOPIC.id}/edit`;
const OPENAI = connectionsFixture.data[0];
const ANTHROPIC = {
  ...OPENAI,
  id: "01900000-0000-7000-8000-000000000401",
  name: "Claude",
  adapter: "anthropic",
  default_model: "claude-sonnet",
};

/** Records the body of the next create request and answers it with `answer`. */
function captureCreate(answer?: () => Response): { body: () => unknown; calls: () => number } {
  let body: unknown;
  let calls = 0;
  server.use(
    http.post("/api/v1/projects/:project/issue-sources", async ({ request }) => {
      calls += 1;
      body = await request.json();
      if (answer) {
        return answer();
      }
      return HttpResponse.json({ ...TOPIC, id: "01900000-0000-7000-8000-000000001c21" }, { status: 201 });
    }),
  );
  return { body: () => body, calls: () => calls };
}

/** Records the body of the next PATCH of a source and answers it with the topic, changed. */
function capturePatch(): { body: () => unknown } {
  let body: unknown;
  server.use(
    http.patch("/api/v1/projects/:project/issue-sources/:id", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...TOPIC, ...(body as Record<string, unknown>) });
    }),
  );
  return { body: () => body };
}

async function choose(user: UserEvent, label: string, option: string) {
  await user.click(screen.getByRole("combobox", { name: label }));
  await user.click(await screen.findByRole("option", { name: option }));
}

/** Fills a topic source's name, connections and models, as a member does on create. */
async function fillTopic(user: UserEvent) {
  await user.type(await screen.findByLabelText("Name"), "Topics");
  await choose(user, "Kind", "Topics");
  await choose(user, "Chat connection", "OpenAI");
  await user.clear(screen.getByLabelText("Chat model"));
  await user.type(screen.getByLabelText("Chat model"), "gpt-5-mini");
  await choose(user, "Embedding connection", "OpenAI");
  await user.type(screen.getByLabelText("Embedding model"), "text-embedding-3-small");
}

it("offers the four kinds on create", async () => {
  const { user } = await renderApp(NEW_URL);

  await user.click(await screen.findByRole("combobox", { name: "Kind" }));

  const options = await screen.findAllByRole("option");
  expect(options.map((option) => option.textContent)).toEqual([
    "Errors",
    "Bad scores",
    "User feedback",
    "Topics",
  ]);
});

it("creates a topic source with its connections, models and defaults", async () => {
  const created = captureCreate();
  const { user, router } = await renderApp(NEW_URL);

  await fillTopic(user);
  await user.click(screen.getByRole("button", { name: "Create source" }));

  await waitFor(() =>
    expect(created.body()).toEqual({
      name: "Topics",
      kind: "topic",
      enabled: true,
      sample_rate: 1,
      daily_cap: 500,
      floor: 0.8,
      min_issue_size: 5,
      chat_connection_id: OPENAI.id,
      chat_model: "gpt-5-mini",
      embed_connection_id: OPENAI.id,
      embed_model: "text-embedding-3-small",
    }),
  );
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("asks for a score name before it sends a bad-score source", async () => {
  const created = captureCreate();
  const { user } = await renderApp(NEW_URL);

  await user.type(await screen.findByLabelText("Name"), "Bad scores");
  await choose(user, "Kind", "Bad scores");
  await user.click(screen.getByRole("button", { name: "Create source" }));

  expect(await screen.findByText("Choose at least one score name.")).toBeInTheDocument();
  expect(created.calls()).toBe(0);
});

it("disables the Anthropic connections in the embedding select", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/llm-connections", () =>
      HttpResponse.json({ data: [OPENAI, ANTHROPIC], next_cursor: null }),
    ),
  );
  const { user } = await renderApp(NEW_URL);

  await user.click(await screen.findByRole("combobox", { name: "Kind" }));
  await user.click(await screen.findByRole("option", { name: "Topics" }));
  await user.click(screen.getByRole("combobox", { name: "Embedding connection" }));

  const claude = await screen.findByRole("option", { name: /Claude/ });
  expect(claude).toHaveTextContent("No embeddings API");
  expect(claude).toHaveAttribute("aria-disabled", "true");
  expect(screen.getByRole("option", { name: "OpenAI" })).not.toHaveAttribute("aria-disabled");
});

it("shows the embedding model read-only on edit, with its hint", async () => {
  server.use(
    http.get("/api/v1/projects/:project/issue-sources/:id", () => HttpResponse.json(TOPIC)),
  );
  await renderApp(EDIT_URL);

  const model = await screen.findByLabelText("Embedding model");
  expect(model).toHaveAttribute("readonly");
  expect(model).toHaveAccessibleDescription("Fixed after create. Create a new source to change them.");
  expect(screen.queryByRole("combobox", { name: "Embedding connection" })).toBeNull();
});

it("sends only the changed LLM fields on edit", async () => {
  server.use(
    http.get("/api/v1/projects/:project/issue-sources/:id", () => HttpResponse.json(TOPIC)),
  );
  const patched = capturePatch();
  const { user } = await renderApp(EDIT_URL);

  const cap = await screen.findByLabelText("Daily cap");
  await user.clear(cap);
  await user.type(cap, "800");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched.body()).toEqual({ daily_cap: 800 }));
});

it("keeps the floor and the minimum size under Advanced until it is opened", async () => {
  const { user } = await renderApp(NEW_URL);

  await choose(user, "Kind", "Topics");
  expect(screen.queryByLabelText("Join floor")).toBeNull();
  expect(screen.queryByLabelText("Minimum issue size")).toBeNull();

  await user.click(screen.getByRole("button", { name: "Advanced" }));

  expect(await screen.findByLabelText("Join floor")).toHaveValue(0.8);
  expect(screen.getByLabelText("Minimum issue size")).toHaveValue(5);
});

it("shows a server field error under its field", async () => {
  captureCreate(() =>
    problemResponse(400, "invalid", {
      errors: [{ field: "embed_model", message: "No such embedding model" }],
    }),
  );
  const { user } = await renderApp(NEW_URL);

  await fillTopic(user);
  await user.click(screen.getByRole("button", { name: "Create source" }));

  expect(await screen.findByText("No such embedding model")).toBeInTheDocument();
  expect(screen.getByLabelText("Embedding model")).toHaveAttribute("aria-invalid", "true");
});
