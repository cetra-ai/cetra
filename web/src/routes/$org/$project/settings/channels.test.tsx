import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { WEBHOOK_CHANNEL_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import channelsFixture from "../../../../../../fixtures/api/static/channels.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const projectNav = () => within(screen.getByRole("list", { name: "Project settings" }));

const PAGE = "/acme/support-bot/settings/channels";
const CHANNELS = "/api/v1/projects/:project/channels";

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

async function findGrid() {
  return await screen.findByRole("grid", { name: "Notification channels" });
}

async function openRow(user: UserEvent, name: string) {
  const grid = await findGrid();
  await user.click(within(grid).getByText(name));
  return await screen.findByRole("form", { name: "Edit channel" });
}

async function openCreate(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "New channel" }));
  return await screen.findByRole("form", { name: "New channel" });
}

it("lists channels with a status pill and the last delivery", async () => {
  await renderApp(PAGE);

  const grid = await findGrid();
  const webhook = within(grid).getByText("Pager webhook").closest('[role="row"]');
  if (!(webhook instanceof HTMLElement)) {
    throw new Error("no row for Pager webhook");
  }
  expect(within(webhook).getByText("hooks.example.com")).toBeInTheDocument();
  expect(within(webhook).getByText("Last delivery failed")).toHaveAttribute(
    "title",
    "the endpoint answered 500",
  );
  expect(within(grid).getByText("Delivered")).toBeInTheDocument();
  expect(within(grid).getByText("No deliveries")).toBeInTheDocument();
  expect(projectNav().getByRole("link", { name: "Channels" })).toHaveAttribute(
    "aria-current",
    "page",
  );
});

it("opens the panel from a row, through the open search param", async () => {
  const user = (await renderApp(PAGE)).user;
  const form = await openRow(user, "Pager webhook");

  expect(within(form).getByRole("textbox", { name: "Name" })).toHaveValue("Pager webhook");
  expect(screen.getByRole("heading", { name: "Pager webhook" })).toBeInTheDocument();
});

it("opens the panel for a channel named in the URL", async () => {
  await renderApp(`${PAGE}?open=${WEBHOOK_CHANNEL_ID}`);

  const form = await screen.findByRole("form", { name: "Edit channel" });
  expect(within(form).getByRole("textbox", { name: "Name" })).toHaveValue("Pager webhook");
});

it("filters the rows by name, kind and target", async () => {
  const user = (await renderApp(PAGE)).user;
  const grid = await findGrid();

  await user.type(screen.getByRole("searchbox", { name: "Search channels" }), "hooks.slack");

  await waitFor(() => expect(within(grid).queryByText("Pager webhook")).toBeNull());
  expect(within(grid).getByText("Team Slack")).toBeInTheDocument();
});

it("sorts by name when the Name header is clicked", async () => {
  const user = (await renderApp(PAGE)).user;
  const grid = await findGrid();

  await user.click(screen.getByRole("button", { name: "Name" }));

  const names = within(grid)
    .getAllByText(/^(Team Slack|Pager webhook|On-call email)$/)
    .map((element) => element.textContent);
  expect(names).toEqual(["Team Slack", "Pager webhook", "On-call email"]);
});

it("creates a webhook channel", async () => {
  let body: unknown;
  let lists = 0;
  const onStart = ({ request }: { request: Request }) => {
    if (request.method === "GET" && new URL(request.url).pathname.endsWith("/channels")) {
      lists += 1;
    }
  };
  server.events.on("request:start", onStart);
  server.use(
    http.post(CHANNELS, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(channelsFixture.data[1], { status: 201 });
    }),
  );

  try {
    const { user } = await renderApp(PAGE);
    await findGrid();
    expect(lists).toBe(1);
    const form = await openCreate(user);
    await user.type(within(form).getByLabelText("Name"), "Pager");
    await user.click(within(form).getByRole("radio", { name: "Webhook" }));
    await user.type(within(form).getByLabelText("URL"), "https://hooks.example.com/x");
    await user.type(within(form).getByLabelText("Signing secret"), "0123456789abcdef");
    await user.click(within(form).getByRole("button", { name: "Add header" }));
    await user.type(within(form).getByLabelText("Header name 1"), "X-Team");
    await user.type(within(form).getByLabelText("Header value 1"), "core");
    await user.click(within(form).getByRole("button", { name: "Create channel" }));

    await waitFor(() =>
      expect(body).toEqual({
        name: "Pager",
        kind: "webhook",
        url: "https://hooks.example.com/x",
        secret: "0123456789abcdef",
        headers: { "X-Team": "core" },
      }),
    );
    await waitFor(() => expect(screen.queryByRole("form", { name: "New channel" })).toBeNull());
    await waitFor(() => expect(lists).toBe(2));
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("creates an email channel", async () => {
  let body: unknown;
  server.use(
    http.post(CHANNELS, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(channelsFixture.data[0], { status: 201 });
    }),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openCreate(user);
  await user.type(within(form).getByLabelText("Name"), "Ops");
  await user.click(within(form).getByRole("radio", { name: "Email" }));
  await user.type(within(form).getByLabelText("Recipients"), "a@x.io\n\n b@x.io ");
  await user.click(within(form).getByRole("button", { name: "Create channel" }));

  await waitFor(() =>
    expect(body).toEqual({
      name: "Ops",
      kind: "email",
      recipients: ["a@x.io", "b@x.io"],
    }),
  );
});

it("shows field errors from the server", async () => {
  server.use(
    http.post(CHANNELS, () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "url", message: "must use https" }],
      }),
    ),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openCreate(user);
  await user.type(within(form).getByLabelText("Name"), "Team");
  await user.type(within(form).getByLabelText("Webhook URL"), "https://x.test/hook");
  await user.click(within(form).getByRole("button", { name: "Create channel" }));

  expect(await within(form).findByText("must use https")).toBeInTheDocument();
  expect(within(form).getByLabelText("Webhook URL")).toHaveAttribute("aria-invalid", "true");
  expect(within(form).queryByRole("alert")).toBeNull();
});

it("shows a required-field error once the field is touched, and keeps Save disabled", async () => {
  let posts = 0;
  server.use(
    http.post(CHANNELS, () => {
      posts += 1;
      return HttpResponse.json(channelsFixture.data[1], { status: 201 });
    }),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openCreate(user);
  const name = within(form).getByLabelText("Name");
  await user.type(name, "x");
  await user.clear(name);

  expect(await within(form).findByText("Enter a name.")).toBeInTheDocument();
  expect(name).toHaveAttribute("aria-invalid", "true");
  expect(within(form).getByRole("button", { name: "Create channel" })).toBeDisabled();
  expect(posts).toBe(0);
});

it("edits without resending secrets", async () => {
  let body: unknown;
  server.use(
    http.patch("/api/v1/projects/:project/channels/:id", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(channelsFixture.data[1]);
    }),
  );

  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "Pager webhook");
  await user.clear(within(form).getByLabelText("Name"));
  await user.type(within(form).getByLabelText("Name"), "Pager 2");
  await user.click(within(form).getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ name: "Pager 2" }));
});

it("warns before discarding unsaved edits when another row is opened", async () => {
  const user = (await renderApp(PAGE)).user;
  const form = await openRow(user, "Pager webhook");
  await user.type(within(form).getByLabelText("Name"), " edited");

  const grid = await findGrid();
  await user.click(within(grid).getByText("Team Slack"));

  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  await waitFor(() =>
    expect(screen.queryByRole("alertdialog", { name: "Discard unsaved changes?" })).toBeNull(),
  );
  expect(screen.getByRole("heading", { name: "Pager webhook" })).toBeInTheDocument();

  await user.click(within(grid).getByText("Team Slack"));
  const again = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(again).getByRole("button", { name: "Discard" }));
  expect(await screen.findByRole("heading", { name: "Team Slack" })).toBeInTheDocument();
});

it("deletes a channel after its name is typed", async () => {
  const ids: string[] = [];
  server.use(
    http.delete("/api/v1/projects/:project/channels/:id", ({ params }) => {
      ids.push(String(params.id));
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await renderApp(PAGE);
  await openRow(user, "Pager webhook");
  await user.click(screen.getByRole("button", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete channel?" });
  const confirm = within(dialog).getByRole("button", { name: "Delete" });
  expect(confirm).toBeDisabled();

  await user.type(within(dialog).getByRole("textbox"), "Pager webhook");
  await user.click(confirm);

  await waitFor(() => expect(ids).toEqual([WEBHOOK_CHANNEL_ID]));
  await waitFor(() =>
    expect(screen.queryByRole("alertdialog", { name: "Delete channel?" })).toBeNull(),
  );
});

it("sends a test and shows the result in the panel", async () => {
  server.use(
    http.post("/api/v1/projects/:project/channels/:id/test", () =>
      HttpResponse.json({ ok: false, error: "the endpoint answered 500" }),
    ),
  );

  const { user } = await renderApp(PAGE);
  await openRow(user, "Pager webhook");
  await user.click(screen.getByRole("button", { name: "Send test" }));

  expect(
    await screen.findByText("The channel did not accept the test message."),
  ).toBeInTheDocument();
  const result = screen.getByText("The channel did not accept the test message.");
  expect(result.parentElement).toHaveTextContent("the endpoint answered 500");

  server.use(
    http.post("/api/v1/projects/:project/channels/:id/test", () => HttpResponse.json({ ok: true })),
  );
  await user.click(screen.getByRole("button", { name: "Send test" }));
  expect(await screen.findByText("The channel accepted the test message.")).toBeInTheDocument();
});

it("viewers can open a channel but not change it", async () => {
  let writes = 0;
  const count = () => {
    writes += 1;
    return HttpResponse.json({}, { status: 500 });
  };
  server.use(
    projectAs("viewer"),
    http.post(CHANNELS, count),
    http.patch("/api/v1/projects/:project/channels/:id", count),
    http.delete("/api/v1/projects/:project/channels/:id", count),
    http.post("/api/v1/projects/:project/channels/:id/test", count),
  );

  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "Team Slack");

  expect(screen.getByRole("button", { name: "New channel" })).toBeDisabled();
  expect(within(form).getByRole("textbox", { name: "Name" })).toBeDisabled();
  expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Send test" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Delete" })).toBeDisabled();
  expect(writes).toBe(0);
});

it("flags an email channel without SMTP", async () => {
  server.use(
    http.get(CHANNELS, () =>
      HttpResponse.json({
        ...channelsFixture,
        data: [{ ...channelsFixture.data[0], available: false }],
      }),
    ),
  );

  await renderApp(PAGE);

  expect(await screen.findByText("SMTP not configured")).toBeInTheDocument();
});

it("shows Forbidden for a 403 and the empty state", async () => {
  server.use(http.get(CHANNELS, () => problemResponse(403, "forbidden")));
  const first = await renderApp(PAGE);
  expect(
    await screen.findByText("You don't have access to this project's channels."),
  ).toBeInTheDocument();
  first.unmount();

  server.use(http.get(CHANNELS, () => HttpResponse.json({ data: [], next_cursor: null })));
  await renderApp(PAGE);
  expect(await screen.findByText("No channels yet")).toBeInTheDocument();
});

it("a failed delete keeps the confirmation open with the error", async () => {
  server.use(
    http.delete("/api/v1/projects/:project/channels/:id", () => problemResponse(500, "internal")),
  );

  const { user } = await renderApp(PAGE);
  await openRow(user, "Pager webhook");
  await user.click(screen.getByRole("button", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete channel?" });
  await user.type(within(dialog).getByRole("textbox"), "Pager webhook");
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  const open = await screen.findByRole("alertdialog", { name: "Delete channel?" });
  expect(await within(open).findByRole("alert")).toHaveTextContent("Internal Server Error");
});

it("a duplicate name is a conflict", async () => {
  server.use(
    http.post(CHANNELS, () =>
      problemResponse(409, "conflict", { detail: "a channel with this name already exists" }),
    ),
  );

  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openCreate(user);
  await user.type(within(form).getByLabelText("Name"), "Team Slack");
  await user.type(
    within(form).getByLabelText("Webhook URL"),
    "https://hooks.slack.com/services/T/B/abcd",
  );
  await user.click(within(form).getByRole("button", { name: "Create channel" }));

  expect(
    await within(form).findByText("a channel with this name already exists"),
  ).toBeInTheDocument();
});

it("edit shows header names, not values", async () => {
  const bodies: unknown[] = [];
  server.use(
    http.patch("/api/v1/projects/:project/channels/:id", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json(channelsFixture.data[1]);
    }),
  );

  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "Pager webhook");
  expect(within(form).getByLabelText("Header name 1")).toHaveValue("X-Team");
  expect(within(form).getByLabelText("Header value 1")).toHaveValue("");

  await user.clear(within(form).getByLabelText("Name"));
  await user.type(within(form).getByLabelText("Name"), "Pager 2");
  await user.click(within(form).getByRole("button", { name: "Save" }));
  await waitFor(() => expect(bodies).toEqual([{ name: "Pager 2" }]));
  await waitFor(() => expect(screen.queryByRole("form", { name: "Edit channel" })).toBeNull());

  const again = await openRow(user, "Pager webhook");
  await user.type(within(again).getByLabelText("Header value 1"), "abc");
  await user.click(within(again).getByRole("button", { name: "Save" }));
  await waitFor(() => expect(bodies[1]).toEqual({ headers: { "X-Team": "abc" } }));
});

it("creates a Slack channel and forgets the URL", async () => {
  let body: unknown;
  server.use(
    http.post(CHANNELS, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(channelsFixture.data[2], { status: 201 });
    }),
  );

  const { user, queryClient } = await renderApp(PAGE);
  await findGrid();
  const form = await openCreate(user);
  await user.type(within(form).getByLabelText("Name"), "Team");
  await user.type(
    within(form).getByLabelText("Webhook URL"),
    "https://hooks.slack.com/services/T/B/abcd",
  );
  await user.click(within(form).getByRole("button", { name: "Create channel" }));

  await waitFor(() =>
    expect(body).toEqual({
      name: "Team",
      kind: "slack",
      url: "https://hooks.slack.com/services/T/B/abcd",
    }),
  );
  await waitFor(() => expect(screen.queryByRole("form", { name: "New channel" })).toBeNull());
  await waitFor(() =>
    expect(
      JSON.stringify(
        queryClient
          .getMutationCache()
          .getAll()
          .map((mutation) => mutation.state.variables ?? null),
      ),
    ).not.toContain("hooks.slack.com/services"),
  );
});

it("edit shows the target hint", async () => {
  const { user } = await renderApp(PAGE);
  const form = await openRow(user, "Team Slack");

  expect(within(form).getByText(/hooks\.slack\.com\/…\/abcd/)).toBeInTheDocument();
});

it("caps the headers at ten", async () => {
  const { user } = await renderApp(PAGE);
  await findGrid();
  const form = await openCreate(user);
  await user.click(within(form).getByRole("radio", { name: "Webhook" }));
  for (let index = 0; index < 10; index += 1) {
    await user.click(within(form).getByRole("button", { name: "Add header" }));
  }

  expect(within(form).getByRole("button", { name: "Add header" })).toBeDisabled();
});
