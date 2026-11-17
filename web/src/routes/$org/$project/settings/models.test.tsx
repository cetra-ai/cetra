import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import modelPricesFixture from "../../../../../../fixtures/api/static/model-prices.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const OWN_PRICE_ID = "01900000-0000-7000-8000-000000000c01";
const OWN_PRICE_NAME = "gpt-4o (negotiated)";

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

/** The Save or Add price button of the open panel. The page's own Add price button is not one. */
function panelButton(name: string): HTMLElement {
  const button = screen
    .getAllByRole("button", { name })
    .find((item) => item.getAttribute("form") === "price-form");
  if (button === undefined) {
    throw new Error(`No ${name} button in the panel`);
  }
  return button;
}

async function openAdd(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Add price" }));
  return await screen.findByRole("form", { name: "Add model price" });
}

async function openOwnRow(user: UserEvent) {
  const own = await screen.findByRole("grid", { name: "Project prices" });
  await user.click(within(own).getByText(OWN_PRICE_NAME));
  return await screen.findByRole("form", { name: "Edit model price" });
}

it("shows project and organization prices", async () => {
  server.use(listRows());

  await renderApp("/acme/support-bot/settings/models");

  const own = await screen.findByRole("grid", { name: "Project prices" });
  expect(within(own).getByText(OWN_PRICE_NAME)).toBeInTheDocument();
  expect(within(own).getByText("openai")).toBeInTheDocument();
  expect(within(own).getByText("$2")).toBeInTheDocument();

  const org = screen.getByRole("grid", { name: "Organization prices" });
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
  await screen.findByRole("grid", { name: "Project prices" });
  const search = screen.getByRole("searchbox", { name: "Search built-in prices" });

  await user.type(search, "g");
  await new Promise((resolve) => setTimeout(resolve, 400));
  expect(seen.some((call) => call.scope === "builtin")).toBe(false);

  await user.type(search, "pt-4o");
  await waitFor(() => expect(seen).toContainEqual({ scope: "builtin", q: "gpt-4o" }));
  const table = await screen.findByRole("grid", { name: "Built-in prices" });
  expect(within(table).getByText("gpt-4o")).toBeInTheDocument();
});

it("searches project prices on the server, through the URL", async () => {
  const seen: { scope: string | null; q: string | null }[] = [];
  server.use(listRows(seen));

  const { user, router } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("grid", { name: "Project prices" });
  await user.type(screen.getByRole("searchbox", { name: "Search project prices" }), "zzz");

  await waitFor(() => expect(seen).toContainEqual({ scope: "project", q: "zzz" }));
  expect(await screen.findByText("No project prices match.")).toBeInTheDocument();
  expect(router.state.location.search).toEqual({ q: "zzz" });
});

it("resolves a model", async () => {
  server.use(listRows());

  const { user } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("grid", { name: "Project prices" });
  await user.type(screen.getByRole("textbox", { name: "Model" }), "gpt-4o");
  await user.type(screen.getByRole("textbox", { name: "Provider" }), "openai");
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
  await screen.findByRole("grid", { name: "Project prices" });
  await user.type(screen.getByRole("textbox", { name: "Model" }), "unknown-model");
  await user.click(screen.getByRole("button", { name: "Check" }));

  expect(
    await screen.findByText(
      "No price matches this model. Its spans get no cost, and a budgeted online rule cannot use it as a judge.",
    ),
  ).toBeInTheDocument();
});

it("opens the price named by the URL in the panel", async () => {
  server.use(listRows());

  await renderApp(`/acme/support-bot/settings/models?open=${OWN_PRICE_ID}`);

  const form = await screen.findByRole("form", { name: "Edit model price" });
  expect(within(form).getByLabelText("Model name")).toHaveValue(OWN_PRICE_NAME);
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
  await screen.findByRole("grid", { name: "Project prices" });
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Model name"), "Local model");
  await user.type(within(form).getByLabelText("Input", { exact: true }), "1");
  await user.click(panelButton("Add price"));

  await waitFor(() =>
    expect(body).toEqual({ model_name: "Local model", price_input: 1, tiers: [] }),
  );
});

it("checks each field as it is typed, and holds Save while one is invalid", async () => {
  server.use(listRows());

  const { user } = await renderApp("/acme/support-bot/settings/models");
  await screen.findByRole("grid", { name: "Project prices" });
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Model name"), "Local model");
  expect(within(form).queryByText("Enter an input or an output price.")).toBeNull();
  expect(panelButton("Add price")).toBeDisabled();

  await user.type(within(form).getByLabelText("Input", { exact: true }), "-1");
  expect(await within(form).findByText("Enter a number of 0 or more.")).toBeInTheDocument();
  expect(panelButton("Add price")).toBeDisabled();
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
  const form = await openOwnRow(user);
  const input = within(form).getByLabelText("Input", { exact: true });
  await user.clear(input);
  await user.type(input, "3");
  await user.click(panelButton("Save"));

  await waitFor(() => expect(patch).toBeDefined());
  expect(patch?.path).toMatch(new RegExp(`/models/${OWN_PRICE_ID}$`));
  expect(patch?.body).toEqual(
    expect.objectContaining({
      model_name: OWN_PRICE_NAME,
      price_input: 3,
      valid_from: "2026-01-01T00:00:00.000000000Z",
    }),
  );
});

it("keeps unsaved edits behind a dialog until the user discards them", async () => {
  server.use(listRows());

  const { user } = await renderApp("/acme/support-bot/settings/models");
  const form = await openOwnRow(user);
  await user.type(within(form).getByLabelText("Model name"), " v2");
  await user.click(screen.getByRole("link", { name: "Manage organization prices" }));

  const dialog = await screen.findByRole("alertdialog");
  expect(within(dialog).getByText("Discard unsaved changes?")).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  expect(screen.getByRole("form", { name: "Edit model price" })).toBeInTheDocument();
});

it("refreshes the list after adding and closes the panel", async () => {
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
  await screen.findByRole("grid", { name: "Project prices" });
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Model name"), "Local model");
  await user.type(within(form).getByLabelText("Input", { exact: true }), "1");
  await user.click(panelButton("Add price"));

  await waitFor(() =>
    expect(
      within(screen.getByRole("grid", { name: "Project prices" })).getByText("Local model"),
    ).toBeInTheDocument(),
  );
  await waitFor(() => expect(screen.queryByRole("form", { name: "Add model price" })).toBeNull());
});

it("lets members read but not change", async () => {
  server.use(listRows(), projectAs("member"));

  const { user } = await renderApp("/acme/support-bot/settings/models");

  const form = await openOwnRow(user);
  expect(within(form).getByLabelText("Model name")).toBeDisabled();
  expect(
    screen.getByText("Only project admins can change prices", { selector: "p" }),
  ).toBeVisible();
  expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
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
  await openOwnRow(user);
  await user.click(screen.getByRole("button", { name: "Delete" }));
  const confirm = await screen.findByRole("alertdialog");
  await user.type(within(confirm).getByRole("textbox"), OWN_PRICE_NAME);
  await user.click(within(confirm).getByRole("button", { name: "Delete" }));

  expect(
    await within(confirm).findByText("Budgeted online rules use this price: Relevance on answers."),
  ).toBeInTheDocument();
});
