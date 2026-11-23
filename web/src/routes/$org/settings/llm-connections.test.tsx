import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import connectionsFixture from "../../../../../fixtures/api/static/llm-connections.json";
import meFixture from "../../../../../fixtures/api/static/me.json";

const CONNECTION_ID = "01900000-0000-7000-8000-000000000400";
const CONNECTION = connectionsFixture.data[0];
const CONNECTION_NAME = "OpenAI";
const BASE_URL_MESSAGE =
  "base_url must be an http or https URL without credentials, fragment or query other than api-version";

const orgNav = () => within(screen.getByRole("list", { name: "Organization settings" }));

function meAs(role: string) {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: meFixture.user,
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role }],
    }),
  );
}

const LIST = "/acme/settings/llm-connections";

/** Opens the panel of the one connection through its row. */
async function openConnection(path = LIST) {
  const rendered = await renderApp(path);
  const grid = await screen.findByRole("grid", { name: "LLM connections" });
  // The name column comes first; the provider cell also reads OpenAI.
  const cell = within(grid).getAllByText(CONNECTION_NAME)[0];
  expect(cell).toBeDefined();
  await rendered.user.click(cell as HTMLElement);
  const form = await screen.findByRole("form", { name: "Edit connection" });
  return { ...rendered, form };
}

/** Opens the add panel from the header button. */
async function openCreate() {
  const rendered = await renderApp(LIST);
  await screen.findByRole("grid", { name: "LLM connections" });
  await rendered.user.click(screen.getByRole("button", { name: "Add connection" }));
  const form = await screen.findByRole("form", { name: "New connection" });
  return { ...rendered, form };
}

function searchOf(router: { state: { location: { search: unknown } } }) {
  return router.state.location.search as Record<string, unknown>;
}

it("lists connections with provider, model and status", async () => {
  await renderApp(LIST);

  const grid = await screen.findByRole("grid", { name: "LLM connections" });
  // The name and the provider both read "OpenAI" (the fixture has no base URL).
  expect(within(grid).getAllByText("OpenAI")).toHaveLength(2);
  expect(within(grid).getByText("gpt-5-mini")).toBeInTheDocument();
  expect(within(grid).getByText("OK")).toBeInTheDocument();
  expect(orgNav().getByRole("link", { name: "LLM connections" })).toHaveAttribute(
    "aria-current",
    "page",
  );
});

it("labels an OpenAI connection with a base URL as OpenAI-compatible", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/llm-connections", () =>
      HttpResponse.json({
        data: [{ ...CONNECTION, base_url: "https://gw.example.com/v1" }],
        next_cursor: null,
      }),
    ),
  );
  await openConnection();

  const grid = screen.getByRole("grid", { name: "LLM connections" });
  expect(within(grid).getByText("OpenAI-compatible")).toBeInTheDocument();

  const panelTitle = screen.getByRole("heading", { level: 2, name: CONNECTION_NAME });
  expect(
    within(panelTitle.parentElement as HTMLElement).getByText("OpenAI-compatible"),
  ).toBeInTheDocument();
});

it("opens the row's panel and writes its id to the URL", async () => {
  const { router } = await openConnection();

  expect(searchOf(router)).toMatchObject({ open: CONNECTION_ID });
  expect(screen.getByRole("button", { name: "Test connection" })).toBeEnabled();
});

it("opens the panel from the URL", async () => {
  await renderApp(`${LIST}?open=${CONNECTION_ID}`);

  expect(await screen.findByRole("form", { name: "Edit connection" })).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("OpenAI");
});

it("filters the list by name, provider or model", async () => {
  const { user } = await renderApp(LIST);
  await screen.findByRole("grid", { name: "LLM connections" });
  const search = screen.getByRole("searchbox", { name: "Search connections" });

  await user.type(search, "anthropic");
  expect(await screen.findByText("No connection matches.")).toBeInTheDocument();

  await user.clear(search);
  await user.type(search, "gpt-5");
  expect(await screen.findByRole("grid", { name: "LLM connections" })).toHaveTextContent("OpenAI");
});

it("sorts the list by a column and keeps the sort in the URL", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/llm-connections", () =>
      HttpResponse.json({
        data: [
          { ...CONNECTION, name: "Zeta" },
          { ...CONNECTION, id: "01900000-0000-7000-8000-000000000401", name: "Alpha" },
        ],
        next_cursor: null,
      }),
    ),
  );

  const { user, router } = await renderApp(LIST);
  const grid = await screen.findByRole("grid", { name: "LLM connections" });
  expect(
    within(grid)
      .getAllByText(/^(Alpha|Zeta)$/)
      .map((node) => node.textContent),
  ).toEqual(["Zeta", "Alpha"]);

  // A new column sorts descending first, then ascending.
  const header = screen.getByRole("columnheader", { name: "Name" });
  await user.click(within(header).getByRole("button"));
  await waitFor(() => expect(searchOf(router)).toMatchObject({ sort: "name", desc: true }));
  expect(
    within(grid)
      .getAllByText(/^(Alpha|Zeta)$/)
      .map((node) => node.textContent),
  ).toEqual(["Zeta", "Alpha"]);

  await user.click(within(header).getByRole("button"));
  await waitFor(() => expect(searchOf(router)).toMatchObject({ sort: "name" }));
  expect(searchOf(router).desc).toBeUndefined();
  expect(
    within(grid)
      .getAllByText(/^(Alpha|Zeta)$/)
      .map((node) => node.textContent),
  ).toEqual(["Alpha", "Zeta"]);
});

it("adds a connection from the add panel", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...CONNECTION, name: "Prod" }, { status: 201 });
    }),
  );

  const { user, form, router } = await openCreate();
  await user.type(within(form).getByLabelText("Name"), "Prod");
  await user.type(within(form).getByLabelText("API key"), "sk-secret");
  await user.type(within(form).getByLabelText("Default model"), "gpt-5-mini");
  await user.click(within(form).getByRole("button", { name: "Add connection" }));

  await waitFor(() =>
    expect(body).toEqual({
      name: "Prod",
      adapter: "openai",
      api_key: "sk-secret",
      default_model: "gpt-5-mini",
    }),
  );
  await waitFor(() => expect(searchOf(router).open).toBeUndefined());
});

it("asks before discarding a dirty form, and keeps editing on request", async () => {
  const { user, router } = await openCreate();
  await user.type(screen.getByRole("textbox", { name: "Name" }), "Prod");

  await user.click(screen.getByRole("button", { name: "Close panel" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Prod");
  expect(searchOf(router).open).toBe("new");

  await user.click(screen.getByRole("button", { name: "Close panel" }));
  const again = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(again).getByRole("button", { name: "Discard" }));
  await waitFor(() => expect(searchOf(router).open).toBeUndefined());
});

it("clears the API key when the add panel opens again", async () => {
  const { user, router } = await openCreate();
  await user.type(screen.getByLabelText("API key"), "sk-secret");
  await user.click(screen.getByRole("button", { name: "Close panel" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Discard" }));
  await waitFor(() => expect(searchOf(router).open).toBeUndefined());
  // The closing panel leaves the DOM once its slide-out ends.
  await waitFor(() => expect(screen.queryByRole("form", { name: "New connection" })).toBeNull());

  await user.click(screen.getByRole("button", { name: "Add connection" }));
  const reopened = await screen.findByRole("form", { name: "New connection" });
  expect(within(reopened).getByLabelText("API key")).toHaveValue("");
});

it("sends one extra header and the picked structured output mode", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...CONNECTION, name: "Prod" }, { status: 201 });
    }),
  );

  const { user, form } = await openCreate();
  await user.type(within(form).getByLabelText("Name"), "Prod");
  await user.type(within(form).getByLabelText("API key"), "sk-secret");
  await user.click(within(form).getByRole("combobox", { name: "Structured output" }));
  await user.click(await screen.findByRole("option", { name: "Tool call" }));
  await user.click(within(form).getByText("Advanced"));
  await user.click(within(form).getByRole("button", { name: "Add header" }));
  await user.type(within(form).getByLabelText("Header name 1"), "x-a");
  const headerValue = within(form).getByLabelText("Header value 1");
  expect(headerValue).toHaveAttribute("type", "password");
  expect(headerValue).toHaveAttribute("autocomplete", "off");
  await user.type(headerValue, "secret-value");
  await user.click(within(form).getByRole("button", { name: "Add connection" }));

  await waitFor(() =>
    expect(body).toEqual({
      name: "Prod",
      adapter: "openai",
      api_key: "sk-secret",
      structured_output: "tool",
      extra_headers: { "x-a": "secret-value" },
    }),
  );
});

it("keeps Save disabled for an empty key and shows the error once the field is touched", async () => {
  let calls = 0;
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections", () => {
      calls += 1;
      return HttpResponse.json(CONNECTION, { status: 201 });
    }),
  );

  const { user, form } = await openCreate();
  await user.type(within(form).getByLabelText("Name"), "Prod");
  const save = within(form).getByRole("button", { name: "Add connection" });
  expect(save).toBeDisabled();
  expect(within(form).queryByText("Enter the API key.")).toBeNull();

  await user.type(within(form).getByLabelText("API key"), "x");
  await user.clear(within(form).getByLabelText("API key"));
  expect(await within(form).findByText("Enter the API key.")).toBeInTheDocument();
  expect(save).toBeDisabled();
  expect(calls).toBe(0);
});

it("edits without resending the key", async () => {
  let body: unknown;
  server.use(
    http.patch("/api/v1/orgs/:org/llm-connections/:id", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...CONNECTION, name: "Renamed" });
    }),
  );

  const { user, form } = await openConnection();
  const name = within(form).getByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Renamed");
  await user.click(within(form).getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ name: "Renamed" }));
});

it("clears the default model with an empty string", async () => {
  let body: unknown;
  server.use(
    http.patch("/api/v1/orgs/:org/llm-connections/:id", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(CONNECTION);
    }),
  );

  const { user, form } = await openConnection();
  await user.clear(within(form).getByLabelText("Default model"));
  await user.click(within(form).getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ default_model: "" }));
});

it("shows a server field error under its field", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "base_url", message: BASE_URL_MESSAGE }],
      }),
    ),
  );

  const { user, form } = await openCreate();
  await user.type(within(form).getByLabelText("Name"), "Prod");
  await user.type(within(form).getByLabelText("API key"), "sk-secret");
  await user.click(within(form).getByRole("button", { name: "Add connection" }));

  expect(await within(form).findByText(BASE_URL_MESSAGE)).toBeInTheDocument();
  expect(within(form).getByLabelText("Base URL")).toHaveAttribute("aria-invalid", "true");
});

it("shows field errors without a rendered field above the submit button", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections", () =>
      problemResponse(400, "bad_request", {
        errors: [
          { field: "adapter", message: "unknown adapter" },
          { field: "structured_output", message: "unknown mode" },
        ],
      }),
    ),
  );

  const { user, form } = await openCreate();
  await user.type(within(form).getByLabelText("Name"), "Prod");
  await user.type(within(form).getByLabelText("API key"), "sk-secret");
  await user.click(within(form).getByRole("button", { name: "Add connection" }));

  expect(await within(form).findByText("unknown adapter unknown mode")).toBeInTheDocument();
});

it("opens the Advanced section when an error lands inside it", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections", () =>
      problemResponse(400, "bad_request", {
        errors: [
          {
            field: "requests_per_minute",
            message: "requests_per_minute must be between 1 and 10000",
          },
        ],
      }),
    ),
  );

  const { user, form } = await openCreate();
  await user.type(within(form).getByLabelText("Name"), "Prod");
  await user.type(within(form).getByLabelText("API key"), "sk-secret");
  await user.click(within(form).getByRole("button", { name: "Add connection" }));

  expect(
    await within(form).findByText("requests_per_minute must be between 1 and 10000"),
  ).toBeInTheDocument();
  expect(within(form).getByText("Advanced").closest("details")).toHaveAttribute("open");
});

it("refuses a requests-per-minute value outside 1 to 10000 before saving", async () => {
  let calls = 0;
  server.use(
    http.patch("/api/v1/orgs/:org/llm-connections/:id", () => {
      calls += 1;
      return HttpResponse.json(CONNECTION);
    }),
  );

  const { user, form } = await openConnection();
  await user.click(within(form).getByText("Advanced"));
  const rpm = within(form).getByLabelText("Requests per minute");
  await user.clear(rpm);
  await user.type(rpm, "20000");

  expect(
    await within(form).findByText("Enter a whole number from 1 to 10000."),
  ).toBeInTheDocument();
  expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  expect(calls).toBe(0);
});

it("shows a 409 detail above the submit button", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections", () =>
      problemResponse(409, "conflict", {
        detail: "a connection with this name already exists",
      }),
    ),
  );

  const { user, form } = await openCreate();
  await user.type(within(form).getByLabelText("Name"), "Prod");
  await user.type(within(form).getByLabelText("API key"), "sk-secret");
  await user.click(within(form).getByRole("button", { name: "Add connection" }));

  const detail = await within(form).findByText("a connection with this name already exists");
  const submit = within(form).getByRole("button", { name: "Add connection" });
  expect(detail.compareDocumentPosition(submit)).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
});

it("tests a connection from its panel", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections/:id/test", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ok: true, model: "gpt-5-mini", latency_ms: 420 });
    }),
  );

  const { user } = await openConnection();
  await user.click(screen.getByRole("button", { name: "Test connection" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("button", { name: "Run test" }));

  expect(
    await within(dialog).findByText("Connection works (420 ms, model gpt-5-mini)"),
  ).toBeInTheDocument();
  await waitFor(() => expect(body).toEqual({ model: "gpt-5-mini" }));
});

it("tests with an empty model sends an empty body", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections/:id/test", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ok: true, model: "gpt-5-mini", latency_ms: 10 });
    }),
  );

  const { user } = await openConnection();
  await user.click(screen.getByRole("button", { name: "Test connection" }));
  const dialog = await screen.findByRole("dialog");
  await user.clear(within(dialog).getByLabelText("Model"));
  await user.click(within(dialog).getByRole("button", { name: "Run test" }));

  await waitFor(() => expect(body).toEqual({}));
});

it("blocks Test while the connection has unsaved edits", async () => {
  const { user, form } = await openConnection();
  await user.type(within(form).getByLabelText("Name"), " edited");

  expect(screen.getByRole("button", { name: "Test connection" })).toBeDisabled();
  expect(
    screen.getByRole("button", { name: "Test connection" }).parentElement,
  ).toHaveAccessibleDescription("Save your changes before testing.");
});

it("shows the rate limit message in the test dialog", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections/:id/test", () =>
      problemResponse(429, "too_many_requests", { detail: "slow down" }),
    ),
  );

  const { user } = await openConnection();
  await user.click(screen.getByRole("button", { name: "Test connection" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("button", { name: "Run test" }));

  expect(await within(dialog).findByText("Too many tests. Wait a minute.")).toBeInTheDocument();
});

it("shows a failed test with its hint", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/llm-connections/:id/test", () =>
      HttpResponse.json({
        ok: false,
        model: "m",
        latency_ms: 5,
        error: { class: "pause", message: "provider status 401" },
      }),
    ),
  );

  const { user } = await openConnection();
  await user.click(screen.getByRole("button", { name: "Test connection" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("button", { name: "Run test" }));

  expect(await within(dialog).findByText("Test failed")).toBeInTheDocument();
  expect(within(dialog).getByText("provider status 401")).toBeInTheDocument();
  expect(
    within(dialog).getByText(
      "The key, quota or model access was refused. Rules that use this connection pause until a test of the default model passes.",
    ),
  ).toBeInTheDocument();
});

it("shows a failing connection's status with its error as the title", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/llm-connections", () =>
      HttpResponse.json({
        data: [{ ...CONNECTION, status: "failing", last_error: "provider status 401" }],
        next_cursor: null,
      }),
    ),
  );

  await renderApp(LIST);

  expect(await screen.findByText("Failing")).toHaveAttribute("title", "provider status 401");
});

it("shows a malformed base URL as stored instead of throwing", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/llm-connections", () =>
      HttpResponse.json({
        data: [{ ...CONNECTION, base_url: "not-a-url" }],
        next_cursor: null,
      }),
    ),
  );

  const { form } = await openConnection();

  expect(within(form).getByLabelText("Base URL")).toHaveValue("not-a-url");
});

it("refuses a header without a value", async () => {
  let calls = 0;
  server.use(
    http.patch("/api/v1/orgs/:org/llm-connections/:id", () => {
      calls += 1;
      return HttpResponse.json(CONNECTION);
    }),
  );

  const { user, form } = await openConnection();
  await user.click(within(form).getByText("Advanced"));
  await user.click(within(form).getByRole("button", { name: "Add header" }));
  await user.type(within(form).getByLabelText("Header name 1"), "x-a");

  expect(await within(form).findByText("Enter a value for each header.")).toBeInTheDocument();
  expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  expect(calls).toBe(0);
});

it("refuses duplicate header names without calling the server", async () => {
  let calls = 0;
  server.use(
    http.patch("/api/v1/orgs/:org/llm-connections/:id", () => {
      calls += 1;
      return HttpResponse.json(CONNECTION);
    }),
  );

  const { user, form } = await openConnection();
  await user.click(within(form).getByText("Advanced"));
  await user.click(within(form).getByRole("button", { name: "Add header" }));
  await user.click(within(form).getByRole("button", { name: "Add header" }));
  await user.type(within(form).getByLabelText("Header name 1"), "x-a");
  await user.type(within(form).getByLabelText("Header value 1"), "1");
  await user.type(within(form).getByLabelText("Header name 2"), "X-A");
  await user.type(within(form).getByLabelText("Header value 2"), "2");

  expect(await within(form).findByText("Duplicate header name.")).toBeInTheDocument();
  expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  expect(calls).toBe(0);
});

it("deletes after typing the connection name", async () => {
  let pathId = "";
  server.use(
    http.delete("/api/v1/orgs/:org/llm-connections/:id", ({ params }) => {
      pathId = String(params.id);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user, router } = await openConnection();
  await user.click(screen.getByRole("button", { name: "Delete…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete connection?" });
  const confirm = within(dialog).getByRole("button", { name: "Delete" });
  expect(confirm).toBeDisabled();
  await user.type(within(dialog).getByRole("textbox"), CONNECTION_NAME);
  await user.click(confirm);

  await waitFor(() => expect(pathId).toBe(CONNECTION_ID));
  await waitFor(() => expect(searchOf(router).open).toBeUndefined());
});

it("shows a failed delete error", async () => {
  server.use(
    http.delete("/api/v1/orgs/:org/llm-connections/:id", () =>
      problemResponse(429, "too_many_requests", { detail: "Wait a minute before deleting." }),
    ),
  );

  const { user } = await openConnection();
  await user.click(screen.getByRole("button", { name: "Delete…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete connection?" });
  await user.type(within(dialog).getByRole("textbox"), CONNECTION_NAME);
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  expect(await within(dialog).findByText("Wait a minute before deleting.")).toBeInTheDocument();
});

it("a member sees the list and the details, but cannot manage", async () => {
  server.use(meAs("member"));

  const { user, router } = await renderApp(LIST);
  const grid = await screen.findByRole("grid", { name: "LLM connections" });
  const add = screen.getByRole("button", { name: "Add connection" });
  expect(add).toBeDisabled();
  expect(add.parentElement).toHaveAccessibleDescription(
    "Only admins and owners can add connections",
  );

  // The name column comes first; the provider cell also reads OpenAI.
  const cell = within(grid).getAllByText(CONNECTION_NAME)[0];
  expect(cell).toBeDefined();
  await user.click(cell as HTMLElement);
  expect(await screen.findByText("Extra headers")).toBeInTheDocument();
  expect(searchOf(router)).toMatchObject({ open: CONNECTION_ID });
  expect(screen.queryByRole("form", { name: "Edit connection" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Test connection" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Delete…" })).toBeNull();
  expect(screen.getByText("••••WXYZ")).toBeInTheDocument();
});
