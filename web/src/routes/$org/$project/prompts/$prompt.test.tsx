import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { invalidatePrompt } from "@/api/queries-prompts";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import labelEventsFixture from "../../../../../../fixtures/api/responses/prompts/support-reply/label-events.json";
import versionOneFixture from "../../../../../../fixtures/api/responses/prompts/support-reply/versions/1.json";
import versionTwoFixture from "../../../../../../fixtures/api/responses/prompts/support-reply/versions/2.json";
import promptFixture from "../../../../../../fixtures/api/responses/prompts/support-reply.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

/** Answers the project read with `role`, so the page sees the signed-in user's role. */
function asRole(role: "member" | "viewer") {
  server.use(
    http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role })),
  );
}

/** The row of a version in the Versions list. */
function rowOf(list: HTMLElement, text: string): HTMLElement {
  const row = within(list).getByText(text).closest("li");
  if (row === null) {
    throw new Error(`no row for ${text}`);
  }
  return row;
}

it("shows the latest version", async () => {
  await renderApp("/acme/support-bot/prompts/support-reply");

  expect(
    await screen.findByRole("heading", { level: 2, name: "support-reply" }),
  ).toBeInTheDocument();
  expect(screen.getByText("Chat")).toBeInTheDocument();
  expect(await screen.findByText(/friendly support agent for/)).toBeInTheDocument();
  const labels = screen.getByRole("group", { name: "Labels of v2" });
  expect(within(labels).getByText("latest")).toBeInTheDocument();
  expect(within(labels).getByText("staging")).toBeInTheDocument();
  expect(screen.getByText("language")).toBeInTheDocument();
  expect(screen.getByText("Placeholder: history")).toBeInTheDocument();
});

it("shows an older version, and goes back to the latest", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp("/acme/support-bot/prompts/support-reply?version=1");

  expect(await screen.findByText("Viewing version 1.")).toBeInTheDocument();
  const labels = screen.getByRole("group", { name: "Labels of v1" });
  expect(await within(labels).findByText("production")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Go to latest" }));
  await waitFor(() => expect(screen.queryByText("Viewing version 1.")).not.toBeInTheDocument());
  expect((router.state.location.search as Record<string, unknown>).version).toBeUndefined();
});

it("answers not found for an unknown prompt", async () => {
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt", () => problemResponse(404, "not_found")),
  );
  await renderApp("/acme/support-bot/prompts/nope");

  expect(await screen.findByRole("heading", { name: "Page not found" })).toBeInTheDocument();
});

it("saves a new version with the edited template and the chosen label", async () => {
  const user = userEvent.setup();
  const requests = recordRequests();
  let latestVersion = 2;
  const savedVersion = {
    ...versionTwoFixture,
    version: 3,
    commit_message: "Shorter",
    labels: ["latest", "production"],
    template: [
      { role: "system", content: "You help the support team." },
      { type: "placeholder", name: "history" },
      { role: "user", content: "{{ question }}" },
    ],
  };
  server.use(
    http.post("/api/v1/projects/:project/prompts/:prompt/versions", () => {
      latestVersion = 3;
      return HttpResponse.json(savedVersion, { status: 201 });
    }),
    http.get("/api/v1/projects/:project/prompts/:prompt", () =>
      HttpResponse.json({ ...promptFixture, latest_version: latestVersion }),
    ),
    http.get("/api/v1/projects/:project/prompts/:prompt/versions/:version", ({ params }) =>
      HttpResponse.json(params.version === "3" ? savedVersion : versionTwoFixture),
    ),
  );

  await renderApp("/acme/support-bot/prompts/support-reply");
  await user.click(await screen.findByRole("button", { name: "Edit" }));
  const system = screen.getByLabelText("Content of message 1");
  await user.clear(system);
  await user.type(system, "You help the support team.");
  await user.click(screen.getByRole("button", { name: "Save as new version…" }));

  const dialog = await screen.findByRole("dialog", { name: "Save as new version" });
  await user.type(within(dialog).getByLabelText("Commit message"), "Shorter");
  await user.click(within(dialog).getByRole("checkbox", { name: "production" }));
  await user.click(within(dialog).getByRole("button", { name: "Save version" }));

  await waitFor(() =>
    expect(requests.calls.find((call) => call.method === "POST")?.body).toBeDefined(),
  );
  expect(requests.calls.find((call) => call.method === "POST")?.body).toMatchObject({
    template: [
      { role: "system", content: "You help the support team." },
      { type: "placeholder", name: "history" },
      { role: "user", content: "{{ question }}" },
    ],
    commit_message: "Shorter",
    labels: ["production"],
  });
  expect(await screen.findByText("You help the support team.")).toBeInTheDocument();
  requests.stop();
});

it("lets a member save, with production disabled and the reason shown", async () => {
  const user = userEvent.setup();
  asRole("member");
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Edit" }));
  await user.click(screen.getByRole("button", { name: "Save as new version…" }));
  const dialog = await screen.findByRole("dialog", { name: "Save as new version" });
  expect(within(dialog).getByRole("checkbox", { name: "production" })).toBeDisabled();
  expect(
    within(dialog).getByText("Only project admins can move protected labels"),
  ).toBeInTheDocument();
  expect(within(dialog).getByRole("checkbox", { name: "staging" })).toBeEnabled();
});

it("shows the diff with the previous version", async () => {
  const user = userEvent.setup();
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("tab", { name: "Versions" }));
  const list = await screen.findByRole("list", { name: "Versions" });
  await user.click(within(rowOf(list, "v2")).getByRole("button", { name: "Diff" }));

  const diff = await screen.findByLabelText("Template diff");
  const removed = within(diff).getByText(/You are a support agent/);
  expect(removed.closest("[data-op]")).toHaveAttribute("data-op", "removed");
  const added = within(diff).getByText(/friendly support agent/);
  expect(added.closest("[data-op]")).toHaveAttribute("data-op", "added");
});

it("sets a label on a version and removes a label", async () => {
  const user = userEvent.setup();
  const requests = recordRequests();
  const removed: string[] = [];
  server.use(
    http.delete("/api/v1/projects/:project/prompts/:prompt/labels/:label", ({ params }) => {
      removed.push(String(params.label));
      return new HttpResponse(null, { status: 204 });
    }),
    http.put("/api/v1/projects/:project/prompts/:prompt/labels/:label", () =>
      HttpResponse.json(promptFixture),
    ),
  );
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Remove label staging" }));
  const confirm = await screen.findByRole("alertdialog", { name: "Remove label staging?" });
  await user.click(within(confirm).getByRole("button", { name: "Remove label" }));
  await waitFor(() => expect(removed).toEqual(["staging"]));

  await user.click(screen.getByRole("tab", { name: "Versions" }));
  const list = await screen.findByRole("list", { name: "Versions" });
  await user.click(within(rowOf(list, "v1")).getByRole("button", { name: "Set label…" }));
  const dialog = await screen.findByRole("dialog", { name: "Set label" });
  await user.type(within(dialog).getByLabelText("Label"), "canary");
  await user.click(within(dialog).getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(requests.calls.find((call) => call.method === "PUT")?.body).toEqual({ version: 1 }),
  );
  expect(
    requests.calls.find((call) => call.method === "PUT")?.path.endsWith("/labels/canary"),
  ).toBe(true);
  requests.stop();
});

it("lists the label history", async () => {
  const user = userEvent.setup();
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("tab", { name: "Labels" }));
  const history = await screen.findByRole("list", { name: "Label history" });
  expect(within(history).getAllByRole("listitem")).toHaveLength(labelEventsFixture.data.length);
  expect(within(history).getByText("new → v2")).toBeInTheDocument();
  expect(within(history).getByText("new → v1")).toBeInTheDocument();
});

it("keeps a viewer read-only, with the reason", async () => {
  const user = userEvent.setup();
  asRole("viewer");
  await renderApp("/acme/support-bot/prompts/support-reply");

  expect(await screen.findByRole("button", { name: "Edit" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Delete prompt" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Remove label staging" })).toBeDisabled();
  expect(screen.getAllByText("Only members can edit prompts").length).toBeGreaterThan(0);

  await user.click(screen.getByRole("tab", { name: "Versions" }));
  const list = await screen.findByRole("list", { name: "Versions" });
  expect(within(rowOf(list, "v1")).getByRole("button", { name: "Set label…" })).toBeDisabled();
});

it("opens a prompt whose name has a dot", async () => {
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt", ({ params }) =>
      HttpResponse.json({ ...promptFixture, name: String(params.prompt), latest_version: 1 }),
    ),
    http.get("/api/v1/projects/:project/prompts/:prompt/versions/:version", ({ params }) =>
      HttpResponse.json({ ...versionOneFixture, name: String(params.prompt) }),
    ),
    http.get("/api/v1/projects/:project/prompts/:prompt/versions", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  await renderApp("/acme/support-bot/prompts/v1.2");

  expect(await screen.findByRole("heading", { level: 2, name: "v1.2" })).toBeInTheDocument();
});

it("asks before dropping unsaved edits when another version is shown", async () => {
  const user = userEvent.setup();
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Edit" }));
  const system = screen.getByLabelText("Content of message 1") as HTMLTextAreaElement;
  const original = system.value;
  await user.type(system, " Be brief.");

  await user.click(screen.getByRole("combobox", { name: "Version" }));
  await user.click(await screen.findByRole("option", { name: "v1" }));
  const confirm = await screen.findByRole("alertdialog", { name: "Discard your edits?" });
  await user.click(within(confirm).getByRole("button", { name: "Cancel" }));
  await waitFor(() =>
    expect(screen.queryByRole("alertdialog", { name: "Discard your edits?" })).toBeNull(),
  );

  // Cancelled: the page stays on the latest version and the edit is still there.
  expect(screen.queryByText("Viewing version 1.")).not.toBeInTheDocument();
  expect(screen.getByLabelText("Content of message 1")).toHaveValue(`${original} Be brief.`);

  await user.click(screen.getByRole("combobox", { name: "Version" }));
  await user.click(await screen.findByRole("option", { name: "v1" }));
  const again = await screen.findByRole("alertdialog", { name: "Discard your edits?" });
  await user.click(within(again).getByRole("button", { name: "Discard" }));
  expect(await screen.findByText("Viewing version 1.")).toBeInTheDocument();
});

it("shows the template and config errors, and keeps the dialog values", async () => {
  const user = userEvent.setup();
  server.use(
    http.post("/api/v1/projects/:project/prompts/:prompt/versions", () =>
      problemResponse(400, "bad_request", {
        errors: [
          {
            field: "template[1].content",
            message: "content is required",
            position: { line: 1, column: 4 },
          },
          { field: "config.params.temperature", message: "must be between 0 and 2" },
        ],
      }),
    ),
  );
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Edit" }));
  await user.click(screen.getByRole("button", { name: "Save as new version…" }));
  const dialog = await screen.findByRole("dialog", { name: "Save as new version" });
  await user.type(within(dialog).getByLabelText("Commit message"), "Shorter");
  await user.click(within(dialog).getByRole("button", { name: "Save version" }));

  // The dialog closes so the editor shows the problems: the placeholder row of message 2 and config.
  const message = await screen.findByText(/content is required/);
  expect(screen.getByLabelText("Name of placeholder 2").closest("li")).toContainElement(message);
  expect(message).toHaveTextContent("(line 1, column 4)");
  expect(screen.getByText("must be between 0 and 2")).toBeInTheDocument();
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: "Save as new version" })).toBeNull(),
  );

  // Editing the template clears its errors; the config error stays until the next save.
  await user.type(screen.getByLabelText("Name of placeholder 2"), "x");
  expect(screen.queryByText(/content is required/)).toBeNull();
  expect(screen.getByText("must be between 0 and 2")).toBeInTheDocument();

  // The dialog keeps what was typed in it.
  await user.click(screen.getByRole("button", { name: "Save as new version…" }));
  const again = await screen.findByRole("dialog", { name: "Save as new version" });
  expect(within(again).getByLabelText("Commit message")).toHaveValue("Shorter");
});

it("keeps an unsaved edit when a newer version is saved meanwhile", async () => {
  const user = userEvent.setup();
  let latestVersion = 2;
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt", () =>
      HttpResponse.json({ ...promptFixture, latest_version: latestVersion }),
    ),
    http.get("/api/v1/projects/:project/prompts/:prompt/versions/:version", ({ params }) =>
      HttpResponse.json(
        params.version === "3"
          ? { ...versionTwoFixture, version: 3, labels: ["latest"] }
          : versionTwoFixture,
      ),
    ),
  );
  const { queryClient } = await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Edit" }));
  const system = screen.getByLabelText("Content of message 1") as HTMLTextAreaElement;
  const original = system.value;
  await user.type(system, " Be brief.");

  latestVersion = 3;
  act(() => {
    invalidatePrompt(queryClient, "acme", "support-bot", "support-reply");
  });

  expect(await screen.findByRole("status")).toHaveTextContent(
    "A newer version exists. Saving creates a version after it.",
  );
  expect(screen.getByLabelText("Content of message 1")).toHaveValue(`${original} Be brief.`);
});

it("shows the problem's detail when a label removal is refused for an unprotected label", async () => {
  const user = userEvent.setup();
  server.use(
    http.delete("/api/v1/projects/:project/prompts/:prompt/labels/:label", () =>
      problemResponse(403, "forbidden", { detail: "Labels are frozen for this project." }),
    ),
  );
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Remove label staging" }));
  const confirm = await screen.findByRole("alertdialog", { name: "Remove label staging?" });
  await user.click(within(confirm).getByRole("button", { name: "Remove label" }));

  expect(
    await within(confirm).findByText("Labels are frozen for this project."),
  ).toBeInTheDocument();
});

it("shows the problem's detail when a label move is refused for an unprotected label", async () => {
  const user = userEvent.setup();
  server.use(
    http.put("/api/v1/projects/:project/prompts/:prompt/labels/:label", () =>
      problemResponse(403, "forbidden", { detail: "Labels are frozen for this project." }),
    ),
  );
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("tab", { name: "Versions" }));
  const list = await screen.findByRole("list", { name: "Versions" });
  await user.click(within(rowOf(list, "v1")).getByRole("button", { name: "Set label…" }));
  const dialog = await screen.findByRole("dialog", { name: "Set label" });
  await user.type(within(dialog).getByLabelText("Label"), "canary");
  await user.click(within(dialog).getByRole("button", { name: "Save" }));

  expect(
    await within(dialog).findByText("Labels are frozen for this project."),
  ).toBeInTheDocument();
});

it("deletes the prompt once its name is typed, and returns to the list", async () => {
  const user = userEvent.setup();
  const requests = recordRequests();
  server.use(
    http.delete("/api/v1/projects/:project/prompts/:prompt", () =>
      new HttpResponse(null, { status: 204 }),
    ),
  );
  const { router } = await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Delete prompt" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete prompt?" });
  await user.type(
    within(dialog).getByLabelText('Type the prompt name "support-reply" to confirm'),
    "support-reply",
  );
  await user.click(within(dialog).getByRole("button", { name: "Delete prompt" }));

  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/prompts"),
  );
  expect(requests.calls.some((call) => call.method === "DELETE")).toBe(true);
  requests.stop();
});

it("reads a deleted prompt again when it is opened after the delete", async () => {
  const user = userEvent.setup();
  const reads: string[] = [];
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt", ({ params }) => {
      reads.push(String(params.prompt));
      return HttpResponse.json(promptFixture);
    }),
    http.delete("/api/v1/projects/:project/prompts/:prompt", () =>
      new HttpResponse(null, { status: 204 }),
    ),
  );
  const { router } = await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("button", { name: "Delete prompt" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete prompt?" });
  await user.type(
    within(dialog).getByLabelText('Type the prompt name "support-reply" to confirm'),
    "support-reply",
  );
  await user.click(within(dialog).getByRole("button", { name: "Delete prompt" }));
  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/prompts"),
  );

  const grid = await screen.findByRole("grid", { name: "Prompts" });
  const before = reads.length;
  await user.click(within(grid).getByText("support-reply"));
  await waitFor(() => expect(reads.length).toBeGreaterThan(before));
});

it("shows the changes of the version shown after a Diff of another version", async () => {
  const user = userEvent.setup();
  await renderApp("/acme/support-bot/prompts/support-reply");

  await user.click(await screen.findByRole("combobox", { name: "Version" }));
  await user.click(await screen.findByRole("option", { name: "v1" }));
  await user.click(await screen.findByRole("tab", { name: "Versions" }));
  const list = await screen.findByRole("list", { name: "Versions" });
  await user.click(within(rowOf(list, "v1")).getByRole("button", { name: "Diff" }));
  expect(await screen.findByRole("heading", { name: "Changes in v1" })).toBeInTheDocument();

  await user.click(screen.getByRole("combobox", { name: "Version" }));
  await user.click(await screen.findByRole("option", { name: "Latest (v2)" }));
  expect(await screen.findByRole("heading", { name: "Changes in v2" })).toBeInTheDocument();
});
