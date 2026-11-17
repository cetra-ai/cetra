import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import projectFixture from "../../../../../../fixtures/api/static/project.json";
import scoreDefinitionsFixture from "../../../../../../fixtures/api/static/score-definitions.json";

const DEFINITION_ID = "01900000-0000-7000-8000-000000000600";
const DEFINITION = scoreDefinitionsFixture.data[0];
const PAGE = "/acme/support-bot/settings/score-definitions";

const projectNav = () => within(screen.getByRole("list", { name: "Project settings" }));

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

async function findGrid() {
  return await screen.findByRole("grid", { name: "Score definitions" });
}

async function openRow(user: UserEvent, name: string) {
  const grid = await findGrid();
  await user.click(within(grid).getByText(name));
  return await screen.findByRole("form", { name: "Edit score definition" });
}

async function openAdd(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Add definition" }));
  return await screen.findByRole("form", { name: "Add score definition" });
}

it("lists the definitions with their range, direction and status", async () => {
  await renderApp(PAGE);

  const grid = await findGrid();
  expect(within(grid).getByText("relevance")).toBeInTheDocument();
  expect(within(grid).getByText("Numeric")).toBeInTheDocument();
  expect(within(grid).getByText("0 – 1")).toBeInTheDocument();
  expect(within(grid).getByText("↑ higher")).toBeInTheDocument();
  expect(within(grid).getByText("Active")).toBeInTheDocument();
  expect(projectNav().getByRole("link", { name: "Score definitions" })).toHaveAttribute(
    "aria-current",
    "page",
  );
});

it("opens the panel from a row, with the description", async () => {
  const user = (await renderApp(PAGE)).user;
  const form = await openRow(user, "relevance");

  expect(within(form).getByLabelText("Description")).toHaveValue(
    "How relevant the answer is, from 0 to 1.",
  );
  expect(screen.getByRole("heading", { name: "relevance" })).toBeInTheDocument();
});

it("opens the panel for a definition named in the URL", async () => {
  await renderApp(`${PAGE}?open=${DEFINITION_ID}`);

  const form = await screen.findByRole("form", { name: "Edit score definition" });
  expect(within(form).getByLabelText("Name")).toHaveValue("relevance");
});

it("searches and sorts the rows", async () => {
  server.use(
    http.get("/api/v1/projects/:project/score-definitions", () =>
      HttpResponse.json({
        data: [DEFINITION, { ...DEFINITION, id: "accuracy-id", name: "accuracy" }],
        next_cursor: null,
      }),
    ),
  );

  const { user } = await renderApp(PAGE);
  const grid = await findGrid();
  expect(within(grid).getByText("accuracy")).toBeInTheDocument();

  await user.type(screen.getByRole("searchbox", { name: "Search score definitions" }), "acc");
  await waitFor(() => expect(within(grid).queryByText("relevance")).toBeNull());
  await user.clear(screen.getByRole("searchbox", { name: "Search score definitions" }));

  await user.click(screen.getByRole("button", { name: "Name" }));
  const names = within(grid)
    .getAllByText(/^(relevance|accuracy)$/)
    .map((element) => element.textContent);
  expect(names).toEqual(["relevance", "accuracy"]);
});

it("creates a numeric definition", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/projects/:project/score-definitions", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...DEFINITION, name: "accuracy" }, { status: 201 });
    }),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Name"), "accuracy");
  await user.type(within(form).getByLabelText("Minimum"), "0");
  await user.type(within(form).getByLabelText("Maximum"), "1");
  await user.click(within(form).getByRole("button", { name: "Add definition" }));

  await waitFor(() =>
    expect(body).toEqual({ name: "accuracy", data_type: "numeric", min_value: 0, max_value: 1 }),
  );
  await waitFor(() => expect(screen.queryByRole("form")).toBeNull());
});

it("refuses a categorical definition with no categories, without calling the server", async () => {
  let calls = 0;
  server.use(
    http.post("/api/v1/projects/:project/score-definitions", () => {
      calls += 1;
      return HttpResponse.json(DEFINITION, { status: 201 });
    }),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Name"), "verdict");
  await user.click(within(form).getByRole("combobox", { name: "Type" }));
  await user.click(await screen.findByRole("option", { name: "Categorical" }));
  await user.click(within(form).getByRole("button", { name: "Add category" }));
  await user.click(within(form).getByRole("button", { name: "Remove category 1" }));

  expect(await within(form).findByText("Add at least one category.")).toBeInTheDocument();
  expect(within(form).getByRole("button", { name: "Add definition" })).toBeDisabled();
  expect(calls).toBe(0);
});

it("offers a direction only once a numeric definition has a range", async () => {
  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openAdd(user);
  await user.click(within(form).getByRole("combobox", { name: "Direction" }));

  const higher = await screen.findByRole("option", { name: "Higher is better" });
  expect(higher).toHaveAttribute("aria-disabled", "true");
  expect(screen.getByRole("option", { name: "No direction" })).not.toHaveAttribute(
    "aria-disabled",
    "true",
  );
  expect(form).toHaveTextContent("Set a range to choose a direction.");
  await user.click(screen.getByRole("option", { name: "No direction" }));

  await user.type(within(form).getByLabelText("Minimum"), "0");
  await user.type(within(form).getByLabelText("Maximum"), "1");
  await user.click(within(form).getByRole("combobox", { name: "Direction" }));
  expect(await screen.findByRole("option", { name: "Higher is better" })).not.toHaveAttribute(
    "aria-disabled",
    "true",
  );
});

it("sends lower when Lower is better is picked", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/projects/:project/score-definitions", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(
        { ...DEFINITION, name: "latency", direction: "lower" },
        { status: 201 },
      );
    }),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Name"), "latency");
  await user.type(within(form).getByLabelText("Minimum"), "0");
  await user.type(within(form).getByLabelText("Maximum"), "10");
  await user.click(within(form).getByRole("combobox", { name: "Direction" }));
  await user.click(await screen.findByRole("option", { name: "Lower is better" }));
  await user.click(within(form).getByRole("button", { name: "Add definition" }));

  await waitFor(() =>
    expect(body).toEqual({
      name: "latency",
      data_type: "numeric",
      min_value: 0,
      max_value: 10,
      direction: "lower",
    }),
  );
});

it("sends only the changed fields on edit", async () => {
  let body: unknown;
  server.use(
    http.patch("/api/v1/projects/:project/score-definitions/:id", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(DEFINITION);
    }),
  );

  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "relevance");
  expect(within(form).getByLabelText("Type")).toHaveAttribute("readonly");
  const description = within(form).getByLabelText("Description");
  await user.clear(description);
  await user.type(description, "Relevance to the question.");
  await user.click(within(form).getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ description: "Relevance to the question." }));
});

it("refuses to clear a range on edit", async () => {
  let calls = 0;
  server.use(
    http.patch("/api/v1/projects/:project/score-definitions/:id", () => {
      calls += 1;
      return HttpResponse.json(DEFINITION);
    }),
  );

  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "relevance");
  await user.clear(within(form).getByLabelText("Maximum"));

  expect(
    await within(form).findByText(
      "A range cannot be cleared. Archive this definition and add a new one.",
    ),
  ).toBeInTheDocument();
  expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  expect(calls).toBe(0);
});

it("warns before leaving a dirty form for another row", async () => {
  server.use(
    http.get("/api/v1/projects/:project/score-definitions", () =>
      HttpResponse.json({
        data: [DEFINITION, { ...DEFINITION, id: "accuracy-id", name: "accuracy" }],
        next_cursor: null,
      }),
    ),
  );

  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "relevance");
  await user.type(within(form).getByLabelText("Description"), " more");

  const grid = await findGrid();
  await user.click(within(grid).getByText("accuracy"));

  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  expect(screen.getByRole("heading", { name: "relevance" })).toBeInTheDocument();
});

it("archives after the name is typed, with a DELETE to the fixture id", async () => {
  let pathId = "";
  server.use(
    http.delete("/api/v1/projects/:project/score-definitions/:id", ({ params }) => {
      pathId = String(params.id);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await renderApp(PAGE);
  await openRow(user, "relevance");
  await user.click(screen.getByRole("button", { name: "Archive" }));
  const dialog = await screen.findByRole("alertdialog");
  expect(within(dialog).getByText("Archive relevance?")).toBeInTheDocument();
  const confirm = within(dialog).getByRole("button", { name: "Archive" });
  expect(confirm).toBeDisabled();
  await user.type(within(dialog).getByRole("textbox"), "relevance");
  await user.click(confirm);

  await waitFor(() => expect(pathId).toBe(DEFINITION_ID));
});

it("requests archived definitions when Show archived is on", async () => {
  const includeArchived: string[] = [];
  server.use(
    http.get("/api/v1/projects/:project/score-definitions", ({ request }) => {
      includeArchived.push(new URL(request.url).searchParams.get("include_archived") ?? "");
      return HttpResponse.json(scoreDefinitionsFixture);
    }),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  expect(includeArchived[0]).toBe("false");

  await user.click(screen.getByRole("switch", { name: "Show archived" }));

  await waitFor(() => expect(includeArchived).toContain("true"));
});

it("restores an archived definition with archived false", async () => {
  let body: unknown;
  server.use(
    http.get("/api/v1/projects/:project/score-definitions", () =>
      HttpResponse.json({
        data: [{ ...DEFINITION, archived_at: "2026-01-02T00:00:00.000000000Z" }],
        next_cursor: null,
      }),
    ),
    http.patch("/api/v1/projects/:project/score-definitions/:id", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(DEFINITION);
    }),
  );

  const { user } = await renderApp(PAGE);
  const grid = await findGrid();
  expect(within(grid).getByText("Archived")).toBeInTheDocument();
  await openRow(user, "relevance");
  await user.click(screen.getByRole("button", { name: "Restore" }));

  await waitFor(() => expect(body).toEqual({ archived: false }));
});

it("shows a 409 detail above the submit button", async () => {
  server.use(
    http.post("/api/v1/projects/:project/score-definitions", () =>
      problemResponse(409, "conflict", {
        detail: "a score definition with this name already exists",
      }),
    ),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Name"), "relevance");
  await user.click(within(form).getByRole("button", { name: "Add definition" }));

  const detail = await within(form).findByText("a score definition with this name already exists");
  const submit = within(form).getByRole("button", { name: "Add definition" });
  expect(detail.compareDocumentPosition(submit)).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
});

it("lets a viewer see the list and the definition, but not change it", async () => {
  server.use(projectAs("viewer"));

  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "relevance");

  expect(within(form).getByLabelText("Name")).toBeDisabled();
  expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Archive" })).toBeDisabled();
  const add = screen.getByRole("button", { name: "Add definition" });
  expect(add).toBeDisabled();
  expect(add.parentElement).toHaveAccessibleDescription(
    "Only members can manage score definitions",
  );
});
