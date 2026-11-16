import { screen, waitFor, within } from "@testing-library/react";
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

async function openAddDialog() {
  const { user } = await renderApp("/acme/settings/models");
  await screen.findByRole("table", { name: "Organization prices" });
  await user.click(screen.getByRole("button", { name: "Add price" }));
  const dialog = await screen.findByRole("dialog");
  return { user, dialog };
}

it("lists organization prices", async () => {
  await renderApp("/acme/settings/models");

  const table = await screen.findByRole("table", { name: "Organization prices" });
  expect(within(table).getByText("Llama 3.1 8B (local)")).toBeInTheDocument();
  expect(within(table).getByText("llama3\\.1:8b")).toBeInTheDocument();
  expect(within(table).getByText("Any")).toBeInTheDocument();
  expect(within(table).getAllByText("$0")).toHaveLength(2);
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

it("adds a price with an exact name", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/models", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...PRICE, model_name: "llama3.1:8b" }, { status: 201 });
    }),
  );

  const { user, dialog } = await openAddDialog();
  await user.type(within(dialog).getByLabelText("Model name"), "llama3.1:8b");
  await user.type(within(dialog).getByLabelText("Input", { exact: true }), "0");
  await user.type(within(dialog).getByLabelText("Output", { exact: true }), "0");
  await user.click(within(dialog).getByRole("button", { name: "Add price" }));

  await waitFor(() =>
    expect(body).toEqual({
      model_name: "llama3.1:8b",
      price_input: 0,
      price_output: 0,
      tiers: [],
    }),
  );
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
});

it("adds a regex price with a tier", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/models", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...PRICE, model_name: "llama3" }, { status: 201 });
    }),
  );

  const { user, dialog } = await openAddDialog();
  await user.type(within(dialog).getByLabelText("Model name"), "llama3");
  await user.click(within(dialog).getByRole("radio", { name: "Regular expression" }));
  await user.type(within(dialog).getByLabelText("Pattern"), "llama3.*");
  await user.type(within(dialog).getByLabelText("Input", { exact: true }), "0.1");
  await user.type(within(dialog).getByLabelText("Output", { exact: true }), "0.2");
  await user.click(within(dialog).getByRole("button", { name: "Add tier" }));
  const tier = within(within(dialog).getByRole("group", { name: "Tier 1" }));
  await user.type(tier.getByLabelText("From input tokens"), "128000");
  await user.type(tier.getByLabelText("Input", { exact: true }), "0.2");
  await user.click(within(dialog).getByRole("button", { name: "Add price" }));

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

it("checks the form before sending", async () => {
  let calls = 0;
  server.use(
    http.post("/api/v1/orgs/:org/models", () => {
      calls += 1;
      return HttpResponse.json(PRICE, { status: 201 });
    }),
  );

  const { user, dialog } = await openAddDialog();
  await user.type(within(dialog).getByLabelText("Model name"), "llama3.1:8b");
  await user.click(within(dialog).getByRole("button", { name: "Add price" }));

  expect(await within(dialog).findByText("Enter an input or an output price.")).toBeInTheDocument();
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

  const { user, dialog } = await openAddDialog();
  await user.type(within(dialog).getByLabelText("Model name"), "llama3");
  await user.click(within(dialog).getByRole("radio", { name: "Regular expression" }));
  await user.type(within(dialog).getByLabelText("Pattern"), "(");
  await user.type(within(dialog).getByLabelText("Input", { exact: true }), "1");
  await user.click(within(dialog).getByRole("button", { name: "Add price" }));

  expect(await within(dialog).findByText("is not a valid regular expression")).toBeInTheDocument();
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
  const table = await screen.findByRole("table", { name: "Organization prices" });
  await user.click(within(table).getByRole("button", { name: "Delete" }));
  const confirm = await screen.findByRole("alertdialog");
  await user.click(within(confirm).getByRole("button", { name: "Delete" }));

  expect(
    await within(confirm).findByText("Budgeted online rules use this price: Relevance on answers."),
  ).toBeInTheDocument();
});

it("members can read but not change", async () => {
  server.use(meAs("member"));

  await renderApp("/acme/settings/models");

  const table = await screen.findByRole("table", { name: "Organization prices" });
  expect(within(table).getByText("Llama 3.1 8B (local)")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
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
  const table = await screen.findByRole("table", { name: "Organization prices" });
  await user.click(within(table).getByRole("button", { name: "Edit" }));
  const dialog = await screen.findByRole("dialog");

  expect(within(dialog).getByRole("radio", { name: "Regular expression" })).toBeChecked();
  expect(within(dialog).getByLabelText("Pattern")).toHaveValue("llama3\\.1:8b");
  await user.clear(within(dialog).getByLabelText("Output", { exact: true }));
  await user.type(within(dialog).getByLabelText("Output", { exact: true }), "0.5");
  await user.click(within(dialog).getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(body).toMatchObject({
      model_name: "Llama 3.1 8B (local)",
      match_pattern: "llama3\\.1:8b",
      price_output: 0.5,
    }),
  );
  expect(path).toBe(PRICE_ID);
});
