import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http, type JsonBodyType } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { serveAnnotationDefinitions } from "@/test/score-definitions";
import addedFixture from "../../../../../../fixtures/api/static/annotation-queue-items-added.json";
import queuesFixture from "../../../../../../fixtures/api/static/annotation-queues.json";
import issueFixture from "../../../../../../fixtures/api/static/issue.json";
import eventsFixture from "../../../../../../fixtures/api/static/issue-events.json";
import occurrencesFixture from "../../../../../../fixtures/api/static/issue-occurrences.json";
import entitlementsFixture from "../../../../../../fixtures/api/static/org-entitlements.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";
import membersFixture from "../../../../../../fixtures/api/static/project-members.json";

type Queue = Schema<"AnnotationQueue">;

const ISSUE_ID = "01900000-0000-7000-8000-000000001c01";
const MERGED_ID = "01900000-0000-7000-8000-000000001c02";
const OTHER_ID = "01900000-0000-7000-8000-000000001c03";
const TITLE = issueFixture.title;
const OTHER_TITLE = "TypeError: retry later";
const PAGE = `/acme/support-bot/issues/${ISSUE_ID}`;
const OTHER = { ...issueFixture, id: OTHER_ID, title: OTHER_TITLE, merged_from: [] };
const QUEUES = "/api/v1/projects/:project/annotation-queues";
const SEND = "/api/v1/projects/:project/issues/:id/queue-items";
const TRACE_QUEUE: Queue = queuesFixture.data[0] as Queue;
const SPAN_QUEUE: Queue = {
  ...TRACE_QUEUE,
  id: "01900000-0000-7000-8000-000000000a02",
  name: "Span review",
  target: "span",
};
const SESSION_QUEUE: Queue = {
  ...TRACE_QUEUE,
  id: "01900000-0000-7000-8000-000000000a03",
  name: "Session review",
  target: "session",
};
const NEW_ID = "01900000-0000-7000-8000-000000000a04";

/** Serves the issue (`body` for the page's issue, `OTHER` for the other one), its occurrences, history and members. */
function serveIssue(body: JsonBodyType = issueFixture) {
  server.use(
    http.get("/api/v1/projects/:project/issues/:id", ({ params }) =>
      HttpResponse.json(params.id === OTHER_ID ? OTHER : body),
    ),
    http.get("/api/v1/projects/:project/issues/:id/occurrences", () =>
      HttpResponse.json(occurrencesFixture),
    ),
    http.get("/api/v1/projects/:project/issues/:id/events", () => HttpResponse.json(eventsFixture)),
    http.get("/api/v1/projects/:project/members", () => HttpResponse.json(membersFixture)),
  );
}

/** Records the body of every PATCH of the issue and answers with the fixture issue. */
function capturePatches(): unknown[] {
  const bodies: unknown[] = [];
  server.use(
    http.patch("/api/v1/projects/:project/issues/:id", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json(issueFixture);
    }),
  );
  return bodies;
}

function roleAs(role: string) {
  server.use(
    http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role })),
  );
}

it("shows the title, the state, the source, the chart, the examples and the history", async () => {
  serveIssue();
  await renderApp(PAGE);

  expect(await screen.findByRole("heading", { name: TITLE })).toBeInTheDocument();
  expect(screen.getByText("Open")).toBeInTheDocument();
  expect(screen.getByText("Errors")).toBeInTheDocument();
  expect(await screen.findByRole("img", { name: "Occurrences per day" })).toBeInTheDocument();
  expect(
    await screen.findByRole("link", { name: "ValueError raised by the parser" }),
  ).toHaveAttribute(
    "href",
    "/acme/support-bot/traces/a1000000000000000000000000000001?span=b100000000000012",
  );
  expect(await screen.findByRole("link", { name: "Trace a1000000" })).toBeInTheDocument();
  expect(await screen.findByText("Another issue was merged into this one")).toBeInTheDocument();
  const created = (await screen.findByText("Created")).closest("li") as HTMLElement;
  expect(within(created).getByText("System")).toBeInTheDocument();
  const merged = (await screen.findByText("Another issue was merged into this one")).closest(
    "li",
  ) as HTMLElement;
  expect(await within(merged).findByText("Ada Example")).toBeInTheDocument();
});

it("Resolve sends the state change", async () => {
  serveIssue();
  const bodies = capturePatches();
  const { user } = await renderApp(PAGE);

  await user.click(await screen.findByRole("button", { name: "Resolve" }));

  await waitFor(() => expect(bodies).toEqual([{ state: "resolved" }]));
});

it("Enter saves an edited title", async () => {
  serveIssue();
  const bodies = capturePatches();
  const { user } = await renderApp(PAGE);

  await user.click(await screen.findByRole("button", { name: "Edit title" }));
  const input = screen.getByRole("textbox", { name: "Title" });
  await user.clear(input);
  await user.type(input, "Renamed issue{Enter}");

  await waitFor(() => expect(bodies).toEqual([{ title: "Renamed issue" }]));
});

it("Escape cancels a title edit without a request", async () => {
  serveIssue();
  const bodies = capturePatches();
  const { user } = await renderApp(PAGE);

  await user.click(await screen.findByRole("button", { name: "Edit title" }));
  await user.type(screen.getByRole("textbox", { name: "Title" }), " draft{Escape}");

  expect(await screen.findByRole("heading", { name: TITLE })).toBeInTheDocument();
  expect(bodies).toEqual([]);
});

it("shows a viewer no state, merge or edit controls, but View traces", async () => {
  roleAs("viewer");
  serveIssue();
  await renderApp(PAGE);

  await screen.findByRole("heading", { name: TITLE });
  expect(screen.queryByRole("button", { name: "Resolve" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Ignore" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Merge into…" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Edit title" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Edit description" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Send to queue" })).toBeNull();
  expect(screen.getByRole("link", { name: "View traces" })).toBeInTheDocument();
});

it("the merge dialog lists the other open issues of the source and posts the chosen target", async () => {
  serveIssue();
  server.use(
    http.get("/api/v1/projects/:project/issues", () =>
      HttpResponse.json({ data: [issueFixture, OTHER], next_cursor: null }),
    ),
  );
  const merges: unknown[] = [];
  server.use(
    http.post("/api/v1/projects/:project/issues/:id/merge", async ({ params, request }) => {
      merges.push({ id: params.id, body: await request.json() });
      return HttpResponse.json(issueFixture);
    }),
  );
  const { user, router } = await renderApp(PAGE);

  await user.click(await screen.findByRole("button", { name: "Merge into…" }));
  const dialog = await screen.findByRole("dialog");
  const listbox = await within(dialog).findByRole("listbox", { name: "Issues to merge into" });
  expect(within(listbox).getAllByRole("option")).toHaveLength(1);
  expect(within(listbox).queryByRole("option", { name: new RegExp(TITLE) })).toBeNull();
  await user.click(within(listbox).getByRole("option", { name: new RegExp(OTHER_TITLE) }));
  await user.click(within(dialog).getByRole("button", { name: "Merge" }));

  await waitFor(() => expect(merges).toEqual([{ id: ISSUE_ID, body: { into: OTHER_ID } }]));
  await waitFor(() =>
    expect(router.state.location.pathname).toBe(`/acme/support-bot/issues/${OTHER_ID}`),
  );
});

it("View traces opens the traces filtered to the issue and the issues merged into it", async () => {
  serveIssue();
  const { user, router } = await renderApp(PAGE);

  await user.click(await screen.findByRole("link", { name: "View traces" }));

  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/traces"));
  expect(router.state.location.search).toMatchObject({
    filter: `has_score('issue:${ISSUE_ID}') OR has_score('issue:${MERGED_ID}')`,
  });
});

it("a merged issue shows the banner to its target and no actions", async () => {
  serveIssue({ ...issueFixture, state: "merged", merged_into: OTHER_ID, merged_from: [] });
  await renderApp(PAGE);

  const banner = await screen.findByRole("status");
  await waitFor(() => expect(within(banner).getByRole("link")).toHaveTextContent(OTHER_TITLE));
  expect(banner).toHaveTextContent(`Merged into ${OTHER_TITLE}`);
  expect(within(banner).getByRole("link")).toHaveAttribute(
    "href",
    `/acme/support-bot/issues/${OTHER_ID}`,
  );
  expect(screen.queryByRole("button", { name: "Resolve" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Merge into…" })).toBeNull();
  expect(screen.queryByRole("link", { name: "View traces" })).toBeNull();
  expect(screen.queryByRole("link", { name: "Create alert" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Add examples to dataset" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Send to queue" })).toBeNull();
});

it("shows Create alert with the issue prefilled", async () => {
  serveIssue();
  await renderApp(PAGE);

  expect(await screen.findByRole("link", { name: "Create alert" })).toHaveAttribute(
    "href",
    `/acme/support-bot/alerts/new?metric=issue_occurrences&issue_id=${ISSUE_ID}`,
  );
});

it("Create alert opens the prefilled rule form", async () => {
  serveIssue();
  const { user, router } = await renderApp(PAGE);

  await user.click(await screen.findByRole("link", { name: "Create alert" }));

  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/alerts/new"));
  expect(await screen.findByRole("combobox", { name: "Metric" })).toHaveTextContent(
    "Issue occurrences",
  );
});

it("shows Add examples to dataset to members", async () => {
  serveIssue();
  await renderApp(PAGE);

  expect(
    await screen.findByRole("button", { name: "Add examples to dataset" }),
  ).toBeInTheDocument();
});

it("shows Create alert and Add examples to dataset to members only", async () => {
  roleAs("viewer");
  serveIssue();
  await renderApp(PAGE);

  await screen.findByRole("heading", { name: TITLE });
  expect(screen.queryByRole("link", { name: "Create alert" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Add examples to dataset" })).toBeNull();
});

it("shows Send to queue to members", async () => {
  serveIssue();
  await renderApp(PAGE);

  expect(await screen.findByRole("button", { name: "Send to queue" })).toBeInTheDocument();
});

/** Serves the queues of the project for every list request. */
function serveQueues(queues: Queue[]) {
  server.use(http.get(QUEUES, () => HttpResponse.json({ data: queues, next_cursor: null })));
}

/** Answers each send with `answer` and records the issue id and the body of each request. */
function captureSends(answer: JsonBodyType = addedFixture, status = 201) {
  const calls: { id: string; body: unknown }[] = [];
  server.use(
    http.post(SEND, async ({ request, params }) => {
      calls.push({ id: String(params.id), body: await request.json() });
      return HttpResponse.json(answer, { status });
    }),
  );
  return calls;
}

/** Opens "Send to queue" and returns its dialog. */
async function openSendDialog(user: UserEvent): Promise<HTMLElement> {
  await user.click(await screen.findByRole("button", { name: "Send to queue" }));
  return screen.findByRole("dialog");
}

/** Picks an option of the queue select inside the dialog. */
async function choose(user: UserEvent, dialog: HTMLElement, option: string) {
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
  await user.click(await screen.findByRole("option", { name: option }));
}

it("lists every queue with its kind in the Send to queue dialog", async () => {
  serveIssue();
  serveQueues([TRACE_QUEUE, SPAN_QUEUE, SESSION_QUEUE]);
  const { user } = await renderApp(PAGE);

  const dialog = await openSendDialog(user);
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));

  expect(
    await screen.findByRole("option", { name: "Relevance review · trace" }),
  ).toBeInTheDocument();
  expect(screen.getByRole("option", { name: "Span review · span" })).toBeInTheDocument();
  expect(screen.getByRole("option", { name: "Session review · session" })).toBeInTheDocument();
});

it("sends the issue to the chosen queue", async () => {
  serveIssue();
  serveQueues([TRACE_QUEUE]);
  const calls = captureSends();
  const { user } = await renderApp(PAGE);

  const dialog = await openSendDialog(user);
  await choose(user, dialog, "Relevance review · trace");
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(await within(dialog).findByText("Added 2 to Relevance review.")).toBeInTheDocument();
  expect(calls).toEqual([{ id: ISSUE_ID, body: { queue: TRACE_QUEUE.id } }]);
  expect(within(dialog).getByRole("link", { name: "Open queue" })).toHaveAttribute(
    "href",
    `/acme/support-bot/annotation-queues/${TRACE_QUEUE.id}`,
  );
});

it("names the occurrences that were not added and why", async () => {
  serveIssue();
  serveQueues([TRACE_QUEUE]);
  captureSends();
  const { user } = await renderApp(PAGE);

  const dialog = await openSendDialog(user);
  await choose(user, dialog, "Relevance review · trace");
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(await within(dialog).findByText("1 not added")).toBeInTheDocument();
  expect(within(dialog).getByText("Occurrence 3")).toBeInTheDocument();
  expect(within(dialog).getByText("the trace was not found")).toBeInTheDocument();
});

it("explains a full queue", async () => {
  serveIssue();
  serveQueues([TRACE_QUEUE]);
  server.use(http.post(SEND, () => problemResponse(422, "too_many_items")));
  const { user } = await renderApp(PAGE);

  const dialog = await openSendDialog(user);
  await choose(user, dialog, "Relevance review · trace");
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(await within(dialog).findByText("The queue is full (100 000 items).")).toBeInTheDocument();
});

it("shows why an issue without occurrences cannot be sent", async () => {
  serveIssue();
  serveQueues([TRACE_QUEUE]);
  server.use(
    http.post(SEND, () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "issue", message: "the issue has no occurrences in the data window" }],
      }),
    ),
  );
  const { user } = await renderApp(PAGE);

  const dialog = await openSendDialog(user);
  await choose(user, dialog, "Relevance review · trace");
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(
    await within(dialog).findByText("the issue has no occurrences in the data window"),
  ).toBeInTheDocument();
});

it("creates a new queue first and sends the issue to it", async () => {
  serveIssue();
  serveQueues([]);
  serveAnnotationDefinitions();
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () => HttpResponse.json(entitlementsFixture)),
    http.post(QUEUES, () =>
      HttpResponse.json({ ...TRACE_QUEUE, id: NEW_ID, name: "Fresh review" }, { status: 201 }),
    ),
  );
  const calls = captureSends();
  const { user } = await renderApp(PAGE);

  const dialog = await openSendDialog(user);
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
  await user.click(await screen.findByRole("option", { name: "New queue…" }));
  const create = await screen.findByRole("dialog", { name: "New annotation queue" });
  await user.type(within(create).getByLabelText("Name"), "Fresh review");
  await user.click(await within(create).findByRole("checkbox", { name: "helpfulness" }));
  await user.click(within(create).getByRole("button", { name: "Create queue" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog", { name: "New annotation queue" })).toBeNull(),
  );
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(await within(dialog).findByText("Added 2 to Fresh review.")).toBeInTheDocument();
  expect(calls).toEqual([{ id: ISSUE_ID, body: { queue: NEW_ID } }]);
});
