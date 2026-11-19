import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import meFixture from "../../../../../fixtures/api/static/me.json";
import orgPricesFixture from "../../../../../fixtures/api/static/org-model-prices.json";

const PRICE_ID = "01900000-0000-7000-8000-000000000c00";
const PRICE = orgPricesFixture.data[0];
const PRICE_NAME = "Llama 3.1 8B (local)";
const RULE_ID = "01900000-0000-7000-8000-000000000d00";

const orgNav = () => within(screen.getByRole("list", { name: "Organization settings" }));

function meAs(role: string) {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: meFixture.user,
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role }],
    }),
  );
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
  await renderedGrid();
  await user.click(screen.getByRole("button", { name: "Add price" }));
  return await screen.findByRole("form", { name: "Add model price" });
}

async function renderedGrid() {
  return await screen.findByRole("grid", { name: "Organization prices" });
}

async function openRow(user: UserEvent, name: string) {
  const grid = await renderedGrid();
  await user.click(within(grid).getByText(name));
  return await screen.findByRole("form", { name: "Edit model price" });
}

it("lists organization prices", async () => {
  await renderApp("/acme/settings/models");

  const grid = await renderedGrid();
  expect(within(grid).getByText(PRICE_NAME)).toBeInTheDocument();
  expect(within(grid).getByText("llama3\\.1:8b")).toBeInTheDocument();
  expect(within(grid).getByText("Any")).toBeInTheDocument();
  expect(within(grid).getAllByText("$0")).toHaveLength(2);
  expect(orgNav().getByRole("link", { name: "Model prices" })).toHaveAttribute(
    "aria-current",
    "page",
  );
});

it("shows the empty state", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/models", () => HttpResponse.json({ data: [], next_cursor: null })),
  );

  await renderApp("/acme/settings/models");

  expect(await screen.findByText("No organization prices")).toBeInTheDocument();
  expect(
    screen.getByText(
      "Built-in prices apply. Add a price for a model they do not cover, such as a local model.",
    ),
  ).toBeInTheDocument();
});

it("searches the organization prices by name, pattern or provider", async () => {
  const { user } = await renderApp("/acme/settings/models");
  await renderedGrid();

  await user.type(screen.getByRole("searchbox", { name: "Search organization prices" }), "zzz");

  expect(await screen.findByText("No organization prices match.")).toBeInTheDocument();
});

it("sorts the organization prices by a price column", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/models", () =>
      HttpResponse.json({
        data: [
          PRICE,
          {
            ...PRICE,
            id: "01900000-0000-7000-8000-000000000c05",
            model_name: "Big model",
            match_pattern: "big",
            price_input: 5,
          },
        ],
        next_cursor: null,
      }),
    ),
  );

  const { user } = await renderApp("/acme/settings/models");
  const grid = await renderedGrid();
  const names = () =>
    within(grid)
      .getAllByText(/^(Big model|Llama 3\.1 8B \(local\))$/)
      .map((element) => element.textContent);

  expect(names()).toEqual([PRICE_NAME, "Big model"]);
  await user.click(within(grid).getByRole("button", { name: "Input" }));
  expect(names()).toEqual(["Big model", PRICE_NAME]);
  await user.click(within(grid).getByRole("button", { name: "Input" }));
  expect(names()).toEqual([PRICE_NAME, "Big model"]);
});

it("opens the price named by the URL in the panel", async () => {
  await renderApp(`/acme/settings/models?open=${PRICE_ID}`);

  const form = await screen.findByRole("form", { name: "Edit model price" });
  expect(within(form).getByLabelText("Model name")).toHaveValue(PRICE_NAME);
});

it("adds a price with an exact name", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/models", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...PRICE, model_name: "llama3.1:8b" }, { status: 201 });
    }),
  );

  const { user } = await renderApp("/acme/settings/models");
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Model name"), "llama3.1:8b");
  await user.type(within(form).getByLabelText("Input", { exact: true }), "0");
  await user.type(within(form).getByLabelText("Output", { exact: true }), "0");
  await user.click(panelButton("Add price"));

  await waitFor(() =>
    expect(body).toEqual({
      model_name: "llama3.1:8b",
      price_input: 0,
      price_output: 0,
      tiers: [],
    }),
  );
  await waitFor(() => expect(screen.queryByRole("form", { name: "Add model price" })).toBeNull());
});

it("adds a regex price with a tier", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/models", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...PRICE, model_name: "llama3" }, { status: 201 });
    }),
  );

  const { user } = await renderApp("/acme/settings/models");
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Model name"), "llama3");
  await user.click(within(form).getByRole("radio", { name: "Regular expression" }));
  await user.type(within(form).getByLabelText("Pattern"), "llama3.*");
  await user.type(within(form).getByLabelText("Input", { exact: true }), "0.1");
  await user.type(within(form).getByLabelText("Output", { exact: true }), "0.2");
  await user.click(within(form).getByRole("button", { name: "Add tier" }));
  const tier = within(within(form).getByRole("group", { name: "Tier 1" }));
  await user.type(tier.getByLabelText("From input tokens"), "128000");
  await user.type(tier.getByLabelText("Input", { exact: true }), "0.2");
  await user.click(panelButton("Add price"));

  await waitFor(() =>
    expect(body).toEqual({
      model_name: "llama3",
      match_pattern: "llama3.*",
      price_input: 0.1,
      price_output: 0.2,
      tiers: [{ min_input_tokens: 128000, price_input: 0.2 }],
    }),
  );
});

it("holds Save until the price is valid, and shows the error once a price field changed", async () => {
  let calls = 0;
  server.use(
    http.post("/api/v1/orgs/:org/models", () => {
      calls += 1;
      return HttpResponse.json(PRICE, { status: 201 });
    }),
  );

  const { user } = await renderApp("/acme/settings/models");
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Model name"), "llama3.1:8b");
  expect(within(form).queryByText("Enter an input or an output price.")).toBeNull();
  expect(panelButton("Add price")).toBeDisabled();

  const input = within(form).getByLabelText("Input", { exact: true });
  await user.type(input, "0");
  await user.clear(input);

  expect(await within(form).findByText("Enter an input or an output price.")).toBeInTheDocument();
  expect(panelButton("Add price")).toBeDisabled();
  expect(calls).toBe(0);
});

it("places server field errors", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/models", () =>
      problemResponse(400, "invalid", {
        errors: [{ field: "match_pattern", message: "is not a valid regular expression" }],
      }),
    ),
  );

  const { user } = await renderApp("/acme/settings/models");
  const form = await openAdd(user);
  await user.type(within(form).getByLabelText("Model name"), "llama3");
  await user.click(within(form).getByRole("radio", { name: "Regular expression" }));
  await user.type(within(form).getByLabelText("Pattern"), "(");
  await user.type(within(form).getByLabelText("Input", { exact: true }), "1");
  await user.click(panelButton("Add price"));

  expect(await within(form).findByText("is not a valid regular expression")).toBeInTheDocument();
});

it("explains a price that budgeted rules need", async () => {
  server.use(
    http.delete("/api/v1/orgs/:org/models/:id", () =>
      problemResponse(409, "conflict", {
        errors: [
          {
            field: `rules.${RULE_ID}`,
            message: "Relevance on answers: the judge model would have no price",
          },
        ],
      }),
    ),
  );

  const { user } = await renderApp("/acme/settings/models");
  await openRow(user, PRICE_NAME);
  await user.click(screen.getByRole("button", { name: "Delete" }));
  const confirm = await screen.findByRole("alertdialog");
  await user.type(within(confirm).getByRole("textbox"), PRICE_NAME);
  await user.click(within(confirm).getByRole("button", { name: "Delete" }));

  expect(
    await within(confirm).findByText("Budgeted online rules use this price: Relevance on answers."),
  ).toBeInTheDocument();
});

it("members can read but not change", async () => {
  server.use(meAs("member"));

  const { user } = await renderApp("/acme/settings/models");

  const form = await openRow(user, PRICE_NAME);
  expect(within(form).getByLabelText("Model name")).toBeDisabled();
  expect(
    screen.getByText("Only admins and owners can change prices", { selector: "p" }),
  ).toBeVisible();
  expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
  const add = screen.getByRole("button", { name: "Add price" });
  expect(add).toBeDisabled();
  expect(add.parentElement).toHaveAccessibleDescription("Only admins and owners can change prices");
});

it("edits a row", async () => {
  let body: unknown;
  let path: unknown;
  server.use(
    http.patch("/api/v1/orgs/:org/models/:id", async ({ request, params }) => {
      body = await request.json();
      path = params.id;
      return HttpResponse.json({ ...PRICE, price_output: 0.5 });
    }),
  );

  const { user } = await renderApp("/acme/settings/models");
  const form = await openRow(user, PRICE_NAME);

  expect(within(form).getByRole("radio", { name: "Regular expression" })).toBeChecked();
  expect(within(form).getByLabelText("Pattern")).toHaveValue("llama3\\.1:8b");
  await user.clear(within(form).getByLabelText("Output", { exact: true }));
  await user.type(within(form).getByLabelText("Output", { exact: true }), "0.5");
  await user.click(panelButton("Save"));

  await waitFor(() =>
    expect(body).toMatchObject({
      model_name: PRICE_NAME,
      match_pattern: "llama3\\.1:8b",
      price_output: 0.5,
    }),
  );
  expect(path).toBe(PRICE_ID);
});
