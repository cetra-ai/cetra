import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import eventsFixture from "../../../../../../fixtures/api/static/issue-events.json";
import occurrencesFixture from "../../../../../../fixtures/api/static/issue-occurrences.json";
import issueFixture from "../../../../../../fixtures/api/static/issue.json";
import issuesFixture from "../../../../../../fixtures/api/static/issues.json";
import membersFixture from "../../../../../../fixtures/api/static/project-members.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const ISSUE_ID = "01900000-0000-7000-8000-000000001c01";
const MERGED_ID = "01900000-0000-7000-8000-000000001c02";
const OTHER_ID = "01900000-0000-7000-8000-000000001c03";
const TITLE = issueFixture.title;
const OTHER_TITLE = "TypeError: retry later";
const PAGE = `/acme/support-bot/issues/${ISSUE_ID}`;
const OTHER = { ...issueFixture, id: OTHER_ID, title: OTHER_TITLE, merged_from: [] };

/** Serves the issue (`body` for the page's issue, `OTHER` for the other one), its occurrences, history and members. */
function serveIssue(body: unknown = issueFixture) {
  server.use(
    http.get("/api/v1/projects/:project/issues/:id", ({ params }) =>
      HttpResponse.json(params.id === OTHER_ID ? OTHER : body),
    ),
    http.get("/api/v1/projects/:project/issues/:id/occurrences", () =>
      HttpResponse.json(occurrencesFixture),
    ),
    http.get("/api/v1/projects/:project/issues/:id/events", () =>
      HttpResponse.json(eventsFixture),
    ),
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
  expect(screen.getByRole("img", { name: "Occurrences per day" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "ValueError raised by the parser" })).toHaveAttribute(
    "href",
    "/acme/support-bot/traces/a1000000000000000000000000000001?span=b100000000000012",
  );
  expect(screen.getByRole("link", { name: "Trace a1000000" })).toBeInTheDocument();
  expect(screen.getByText("Another issue was merged into this one")).toBeInTheDocument();
  expect(screen.getByText("Created")).toBeInTheDocument();
  expect(screen.getByText("Ada Example")).toBeInTheDocument();
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
});
