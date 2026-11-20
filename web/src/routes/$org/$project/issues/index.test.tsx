import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http, type JsonBodyType } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import issueSourcesFixture from "../../../../../../fixtures/api/static/issue-sources.json";
import issuesFixture from "../../../../../../fixtures/api/static/issues.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const PAGE = "/acme/support-bot/issues";
const ERRORS_SOURCE = "01900000-0000-7000-8000-000000001c00";
const TITLE = "ValueError: could not parse the reply";
const EMPTY = { data: [], next_cursor: null };

/** Serves the issue and source fixtures, and records every issue list request's URL. */
function serveIssues(body: JsonBodyType = issuesFixture): URL[] {
  const seen: URL[] = [];
  server.use(
    http.get("/api/v1/projects/:project/issues", ({ request }) => {
      seen.push(new URL(request.url));
      return HttpResponse.json(body);
    }),
    http.get("/api/v1/projects/:project/issue-sources", () =>
      HttpResponse.json(issueSourcesFixture),
    ),
  );
  return seen;
}

function roleAs(role: string) {
  server.use(
    http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role })),
  );
}

/** The grid row whose title cell reads `title`. */
function rowOf(grid: HTMLElement, title: string): HTMLElement {
  const row = within(grid).getByText(title).closest<HTMLElement>('[role="row"]');
  if (!row) {
    throw new Error(`no row titled ${title}`);
  }
  return row;
}

async function choose(user: UserEvent, label: string, option: string) {
  await user.click(screen.getByRole("combobox", { name: label }));
  await user.click(await screen.findByRole("option", { name: option }));
}

it("lists the issues with their source, state, counts and first and last seen", async () => {
  serveIssues();
  await renderApp(PAGE);

  const grid = await screen.findByRole("grid", { name: "Issues" });
  const row = rowOf(grid, TITLE);
  expect(within(row).getByText("Errors")).toBeInTheDocument();
  expect(within(row).getByText("Open")).toBeInTheDocument();
  expect(within(row).getByText("12 occurrences · 3 users")).toBeInTheDocument();
  expect(within(row).queryByText("Regressed")).toBeNull();
  expect(within(grid).getByRole("columnheader", { name: "Last seen" })).toBeInTheDocument();
  expect(within(grid).getByRole("columnheader", { name: "First seen" })).toBeInTheDocument();
});

it("marks a regressed issue with a Regressed badge", async () => {
  serveIssues({ ...issuesFixture, data: [{ ...issuesFixture.data[0], regressed: true }] });
  await renderApp(PAGE);

  const grid = await screen.findByRole("grid", { name: "Issues" });
  expect(within(rowOf(grid, TITLE)).getByText("Regressed")).toBeInTheDocument();
});

it("asks for the open issues first, and switching the tab sends its state", async () => {
  const seen = serveIssues();
  const { router, user } = await renderApp(PAGE);
  await screen.findByRole("grid", { name: "Issues" });
  expect(seen.at(-1)?.searchParams.get("state")).toBe("open");

  await user.click(screen.getByRole("tab", { name: "Resolved" }));

  await waitFor(() => expect(seen.at(-1)?.searchParams.get("state")).toBe("resolved"));
  expect(router.state.location.search).toMatchObject({ state: "resolved" });
});

it("the source filter sends the source id and writes it to the URL", async () => {
  const seen = serveIssues();
  const { router, user } = await renderApp(PAGE);
  await screen.findByRole("grid", { name: "Issues" });

  await choose(user, "Source", "Errors");

  await waitFor(() => expect(seen.at(-1)?.searchParams.get("source_id")).toBe(ERRORS_SOURCE));
  expect(router.state.location.search).toMatchObject({ source: ERRORS_SOURCE });
});

it("the search box waits for the typing to stop, then sends q and writes it to the URL", async () => {
  const seen = serveIssues();
  const { router, user } = await renderApp(PAGE);
  await screen.findByRole("grid", { name: "Issues" });

  await user.type(screen.getByRole("textbox", { name: "Search issues" }), "could");

  await waitFor(() => expect(router.state.location.search).toMatchObject({ q: "could" }));
  await waitFor(() => expect(seen.at(-1)?.searchParams.get("q")).toBe("could"));
});

it("the sort select sends the sort and writes it to the URL", async () => {
  const seen = serveIssues();
  const { router, user } = await renderApp(PAGE);
  await screen.findByRole("grid", { name: "Issues" });

  await choose(user, "Sort", "First seen");

  await waitFor(() => expect(seen.at(-1)?.searchParams.get("sort")).toBe("first_seen"));
  expect(router.state.location.search).toMatchObject({ sort: "first_seen" });
});

it("shows the empty state with a link to Issue sources when the user can manage evaluation", async () => {
  roleAs("member");
  serveIssues(EMPTY);
  await renderApp(PAGE);

  const empty = (await screen.findByText("No issues yet")).parentElement
    ?.parentElement as HTMLElement;
  expect(screen.getByText("Add an error source to start finding issues.")).toBeInTheDocument();
  expect(within(empty).getByRole("link", { name: "Issue sources" })).toHaveAttribute(
    "href",
    "/acme/support-bot/issue-sources",
  );
});

it("shows the empty state without the link for a viewer", async () => {
  roleAs("viewer");
  serveIssues(EMPTY);
  await renderApp(PAGE);

  const empty = (await screen.findByText("No issues yet")).parentElement
    ?.parentElement as HTMLElement;
  expect(within(empty).queryByRole("link", { name: "Issue sources" })).toBeNull();
});

it("says no issues match, not that there are none, when a filter leaves nothing", async () => {
  serveIssues(EMPTY);
  await renderApp(`${PAGE}?state=resolved`);

  expect(await screen.findByText("No issues match.")).toBeInTheDocument();
  expect(screen.queryByText("No issues yet")).toBeNull();
});

it("the kind filter keeps the issues of the chosen kind, found through their source", async () => {
  serveIssues();
  const { user } = await renderApp(PAGE);

  await screen.findByRole("grid", { name: "Issues" });
  await choose(user, "Kind", "Topics");
  expect(await screen.findByText("No issues match.")).toBeInTheDocument();

  await choose(user, "Kind", "Errors");
  const grid = await screen.findByRole("grid", { name: "Issues" });
  expect(within(grid).getByText(TITLE)).toBeInTheDocument();
});
