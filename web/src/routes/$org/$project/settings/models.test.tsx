import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import modelPricesFixture from "../../../../../../fixtures/api/static/model-prices.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const projectNav = () => within(screen.getByRole("list", { name: "Project settings" }));

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

/** Answers the list per `scope` and `q`, from the fixture's rows. */
function listRows(seen: { scope: string | null; q: string | null }[] = []) {
  return http.get("/api/v1/projects/:project/models", ({ request }) => {
    const params = new URL(request.url).searchParams;
    const scope = params.get("scope");
    const q = params.get("q");
    seen.push({ scope, q });
    const rows = modelPricesFixture.data.filter(
      (row) =>
        row.scope === scope &&
        (q === null || row.model_name.toLowerCase().includes(q.toLowerCase())),
    );
    return HttpResponse.json({ data: rows, next_cursor: null });
  });
}

async function openAdd(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Add price" }));
  return await screen.findByRole("dialog", { name: "Add model price" });
}

it("shows project and organization prices", async () => {
  server.use(listRows());

  await renderApp("/acme/support-bot/settings/models");

  const own = await screen.findByRole("table", { name: "Project prices" });
  expect(within(own).getByText("gpt-4o (negotiated)")).toBeInTheDocument();
  expect(within(own).getByText("openai")).toBeInTheDocument();
  expect(within(own).getByText("$2")).toBeInTheDocument();

  const org = screen.getByRole("table", { name: "Organization prices" });
  expect(within(org).getByText("Llama 3.1 8B (local)")).toBeInTheDocument();
  expect(within(org).getByText("Any")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Manage organization prices" })).toHaveAttribute(
    "href",
    "/acme/settings/models",
  );
  expect(projectNav().getByRole("link", { name: "Model prices" })).toHaveAttribute(
    "aria-current",
    "page",
  );
});

it("searches built-in prices after a debounce, from two characters", async () => {
  const seen: { scope: string | null; q: string | null }[] = [];
  server.use(listRows(seen));

  const { user } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("table", { name: "Project prices" });
  const search = screen.getByRole("textbox", { name: "Search built-in prices" });

  await user.type(search, "g");
  await new Promise((resolve) => setTimeout(resolve, 400));
  expect(seen.some((call) => call.scope === "builtin")).toBe(false);

  await user.type(search, "pt-4o");
  await waitFor(() => expect(seen).toContainEqual({ scope: "builtin", q: "gpt-4o" }));
  const table = await screen.findByRole("table", { name: "Built-in prices" });
  expect(within(table).getByText("gpt-4o")).toBeInTheDocument();
});

it("resolves a model", async () => {
  server.use(listRows());

  const { user } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("table", { name: "Project prices" });
  await user.type(screen.getByLabelText("Model"), "gpt-4o");
  await user.type(screen.getByLabelText("Provider"), "openai");
  await user.click(screen.getByRole("button", { name: "Check" }));

  expect(
    await screen.findByText(
      "Priced by the project price “gpt-4o (negotiated)”: input $2, output $8 per 1M tokens.",
    ),
  ).toBeInTheDocument();
});

it("says when no price matches", async () => {
  server.use(
    listRows(),
    http.get("/api/v1/projects/:project/models/resolve", () => HttpResponse.json({})),
  );

  const { user } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("table", { name: "Project prices" });
  await user.type(screen.getByLabelText("Model"), "unknown-model");
  await user.click(screen.getByRole("button", { name: "Check" }));

  expect(
    await screen.findByText(
      "No price matches this model. Its spans get no cost, and a budgeted online rule cannot use it as a judge.",
    ),
  ).toBeInTheDocument();
});

it("adds a project price", async () => {
  let body: unknown;
  server.use(
    listRows(),
    http.post("/api/v1/projects/:project/models", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(modelPricesFixture.data[0], { status: 201 });
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("table", { name: "Project prices" });
  const dialog = await openAdd(user);
  await user.type(within(dialog).getByLabelText("Model name"), "Local model");
  await user.type(within(dialog).getByLabelText("Input", { exact: true }), "1");
  await user.click(within(dialog).getByRole("button", { name: "Add price" }));

  await waitFor(() =>
    expect(body).toEqual({ model_name: "Local model", price_input: 1, tiers: [] }),
  );
});

it("edits a project price", async () => {
  let patch: { path: string; body: unknown } | undefined;
  server.use(
    listRows(),
    http.patch("/api/v1/projects/:project/models/:id", async ({ request }) => {
      patch = { path: new URL(request.url).pathname, body: await request.json() };
      return HttpResponse.json(modelPricesFixture.data[0]);
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/models");
  const own = await screen.findByRole("table", { name: "Project prices" });
  await user.click(within(own).getByRole("button", { name: "Edit" }));
  const dialog = await screen.findByRole("dialog", { name: "Edit model price" });
  const input = within(dialog).getByLabelText("Input", { exact: true });
  await user.clear(input);
  await user.type(input, "3");
  await user.click(within(dialog).getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patch).toBeDefined());
  expect(patch?.path).toMatch(/\/models\/01900000-0000-7000-8000-000000000c01$/);
  expect(patch?.body).toEqual(
    expect.objectContaining({
      model_name: "gpt-4o (negotiated)",
      price_input: 3,
      valid_from: "2026-01-01T00:00:00.000000000Z",
    }),
  );
});

it("refreshes the list after adding", async () => {
  let added = false;
  const created = {
    ...modelPricesFixture.data[0],
    id: "01900000-0000-7000-8000-000000000c02",
    model_name: "Local model",
  };
  server.use(
    http.get("/api/v1/projects/:project/models", ({ request }) => {
      const scope = new URL(request.url).searchParams.get("scope");
      const rows = modelPricesFixture.data.filter((row) => row.scope === scope);
      const extra = added && scope === "project" ? [created] : [];
      return HttpResponse.json({ data: [...rows, ...extra], next_cursor: null });
    }),
    http.post("/api/v1/projects/:project/models", () => {
      added = true;
      return HttpResponse.json(created, { status: 201 });
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("table", { name: "Project prices" });
  const dialog = await openAdd(user);
  await user.type(within(dialog).getByLabelText("Model name"), "Local model");
  await user.type(within(dialog).getByLabelText("Input", { exact: true }), "1");
  await user.click(within(dialog).getByRole("button", { name: "Add price" }));

  await waitFor(() =>
    expect(
      within(screen.getByRole("table", { name: "Project prices" })).getByText("Local model"),
    ).toBeInTheDocument(),
  );
});

it("lets members read but not change", async () => {
  server.use(listRows(), projectAs("member"));

  await renderApp("/acme/support-bot/settings/models");

  const own = await screen.findByRole("table", { name: "Project prices" });
  expect(within(own).getByText("gpt-4o (negotiated)")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
  const add = screen.getByRole("button", { name: "Add price" });
  expect(add).toBeDisabled();
  expect(add.parentElement).toHaveAccessibleDescription("Only project admins can change prices");
});

it("explains a delete that budgeted rules block", async () => {
  server.use(
    listRows(),
    http.delete("/api/v1/projects/:project/models/:id", () =>
      problemResponse(409, "conflict", {
        errors: [
          {
            field: "rules.01900000-0000-7000-8000-000000000d00",
            message: "Relevance on answers: the judge model would have no price",
          },
        ],
      }),
    ),
  );

  const { user } = await renderApp("/acme/support-bot/settings/models");
  const own = await screen.findByRole("table", { name: "Project prices" });
  await user.click(within(own).getByRole("button", { name: "Delete" }));
  const confirm = await screen.findByRole("alertdialog");
  await user.click(within(confirm).getByRole("button", { name: "Delete" }));

  expect(
    await within(confirm).findByText("Budgeted online rules use this price: Relevance on answers."),
  ).toBeInTheDocument();
});
