import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import versionOneFixture from "../../../../../../fixtures/api/responses/prompts/support-reply/versions/1.json";
import promptsFixture from "../../../../../../fixtures/api/responses/prompts.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

/** Answers the project read with `role`, so the page sees the signed-in user's role. */
function asRole(role: "member" | "viewer") {
  server.use(
    http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role })),
  );
}

it("lists the prompts with their labels, and locks the protected ones", async () => {
  await renderApp("/acme/support-bot/prompts");
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Prompts"]);

  const grid = await screen.findByRole("grid", { name: "Prompts" });
  expect(within(grid).getByText("support-reply")).toBeInTheDocument();
  expect(within(grid).getByText("summarize")).toBeInTheDocument();
  expect(within(grid).getByText("staging")).toBeInTheDocument();
  // Both prompts point production at a version, and production is protected by default.
  await waitFor(() => expect(within(grid).getAllByLabelText("Protected")).toHaveLength(2));
});

it("filters by search, label and tag, sending each to the list", async () => {
  const user = userEvent.setup();
  const queries: URLSearchParams[] = [];
  server.use(
    http.get("/api/v1/projects/:project/prompts", ({ request }) => {
      queries.push(new URL(request.url).searchParams);
      return HttpResponse.json(promptsFixture);
    }),
  );

  await renderApp("/acme/support-bot/prompts");
  await screen.findByRole("grid", { name: "Prompts" });
  await user.type(screen.getByRole("textbox", { name: "Search prompts" }), "supp");
  await waitFor(() => expect(queries.some((query) => query.get("q") === "supp")).toBe(true));

  await user.click(screen.getByRole("combobox", { name: "Label" }));
  await user.click(await screen.findByRole("option", { name: "staging" }));
  await user.click(screen.getByRole("combobox", { name: "Tag" }));
  await user.click(await screen.findByRole("option", { name: "support" }));

  await waitFor(() => expect(queries.at(-1)?.get("label")).toBe("staging"));
  expect(queries.at(-1)?.get("tag")).toBe("support");
  expect(queries.at(-1)?.get("q")).toBe("supp");
});

it("opens a prompt when its row is activated", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp("/acme/support-bot/prompts");

  const grid = await screen.findByRole("grid", { name: "Prompts" });
  await user.click(within(grid).getByText("support-reply"));

  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/prompts/support-reply"),
  );
});

it("creates a chat prompt with its messages and the production label", async () => {
  const user = userEvent.setup();
  const requests = recordRequests();
  server.use(
    http.post("/api/v1/projects/:project/prompts", () =>
      HttpResponse.json({ ...versionOneFixture, name: "faq-bot" }, { status: 201 }),
    ),
    http.get("/api/v1/projects/:project/prompts/:prompt", ({ params }) =>
      HttpResponse.json({ ...promptsFixture.data[1], name: params.prompt }),
    ),
  );

  const { router } = await renderApp("/acme/support-bot/prompts?create=1");
  const dialog = await screen.findByRole("dialog", { name: "New prompt" });
  await user.type(within(dialog).getByLabelText("Name"), "faq-bot");
  await user.click(within(dialog).getByRole("radio", { name: "Chat" }));
  await user.type(within(dialog).getByLabelText("Content of message 1"), "You help.");
  await user.type(within(dialog).getByLabelText("Content of message 2"), "Question");
  await user.click(within(dialog).getByRole("button", { name: "Create prompt" }));

  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/prompts/faq-bot"),
  );
  const post = requests.calls.find((call) => call.method === "POST");
  expect(post?.body).toEqual({
    name: "faq-bot",
    type: "chat",
    template: [
      { role: "system", content: "You help." },
      { role: "user", content: "Question" },
    ],
    labels: ["production"],
  });
  requests.stop();
});

it("shows a field error under the message it belongs to", async () => {
  const user = userEvent.setup();
  server.use(
    http.post("/api/v1/projects/:project/prompts", () =>
      problemResponse(400, "bad_request", {
        errors: [
          {
            field: "template[1].content",
            message: "content is required",
            position: { line: 1, column: 4 },
          },
        ],
      }),
    ),
  );

  await renderApp("/acme/support-bot/prompts?create=1");
  const dialog = await screen.findByRole("dialog", { name: "New prompt" });
  await user.type(within(dialog).getByLabelText("Name"), "faq-bot");
  await user.click(within(dialog).getByRole("radio", { name: "Chat" }));
  await user.click(within(dialog).getByRole("button", { name: "Create prompt" }));

  const message = await within(dialog).findByText(/content is required/);
  const entry = within(dialog).getByLabelText("Content of message 2").closest("li");
  expect(entry).toContainElement(message);
  expect(message).toHaveTextContent("(line 1, column 4)");
});

it("disables the New prompt button for a viewer, with the reason", async () => {
  asRole("viewer");
  await renderApp("/acme/support-bot/prompts");

  expect(await screen.findByRole("button", { name: "New prompt" })).toBeDisabled();
  expect(screen.getByText("Only members can edit prompts")).toBeInTheDocument();
});

it("lets an admin edit the protected labels, one per line", async () => {
  const user = userEvent.setup();
  const requests = recordRequests();
  server.use(
    http.put("/api/v1/projects/:project/prompt-settings", () =>
      HttpResponse.json({ protected_labels: ["production", "prod"] }),
    ),
  );
  await renderApp("/acme/support-bot/prompts");

  await user.click(await screen.findByRole("button", { name: "Protected labels" }));
  const dialog = await screen.findByRole("dialog", { name: "Protected labels" });
  const field = within(dialog).getByRole("textbox", { name: "Protected labels" });
  await waitFor(() => expect(field).toHaveValue("production"));
  await user.clear(field);
  await user.type(field, "production{enter}prod");
  await user.click(within(dialog).getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(requests.calls.find((call) => call.method === "PUT")?.body).toEqual({
      protected_labels: ["production", "prod"],
    }),
  );
  requests.stop();
});

it("disables the protected labels button below admin, with the reason", async () => {
  asRole("member");
  await renderApp("/acme/support-bot/prompts");

  expect(await screen.findByRole("button", { name: "Protected labels" })).toBeDisabled();
  expect(screen.getByText("Only project admins can change protected labels")).toBeInTheDocument();
});

it("unchecks and disables production for a member, and sends no labels", async () => {
  const user = userEvent.setup();
  asRole("member");
  const requests = recordRequests();
  server.use(
    http.post("/api/v1/projects/:project/prompts", () =>
      HttpResponse.json({ ...versionOneFixture, name: "faq-bot" }, { status: 201 }),
    ),
  );

  await renderApp("/acme/support-bot/prompts?create=1");
  const dialog = await screen.findByRole("dialog", { name: "New prompt" });
  // The disabled box is wrapped by its reason once the settings load, so it is a new element.
  await waitFor(() =>
    expect(within(dialog).getByRole("checkbox", { name: "Label as production" })).toBeDisabled(),
  );
  expect(within(dialog).getByRole("checkbox", { name: "Label as production" })).not.toBeChecked();

  await user.type(within(dialog).getByLabelText("Name"), "faq-bot");
  await user.click(within(dialog).getByRole("button", { name: "Create prompt" }));

  await waitFor(() =>
    expect(requests.calls.find((call) => call.method === "POST")?.body).toMatchObject({
      labels: [],
    }),
  );
  requests.stop();
});

it("shows the empty state texts when the project has no prompts", async () => {
  server.use(
    http.get("/api/v1/projects/:project/prompts", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  await renderApp("/acme/support-bot/prompts");

  expect(await screen.findByText("No prompts yet")).toBeInTheDocument();
  expect(
    screen.getByText("Create a prompt here, or with client.prompts.create in the SDK."),
  ).toBeInTheDocument();
  expect(screen.queryByRole("grid", { name: "Prompts" })).toBeNull();
});
