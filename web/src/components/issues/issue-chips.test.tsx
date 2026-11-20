import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const TRACE = "a1000000000000000000000000000001";
const OPEN = "01900000-0000-7000-8000-0000000000a1";
const RESOLVED = "01900000-0000-7000-8000-0000000000a2";
const LONG = "01900000-0000-7000-8000-0000000000a3";
const LONG_TITLE = "The payments tool timed out while retrying the order after a reset";

const BASE = {
  project_id: "01900000-0000-7000-8000-000000000001",
  target: { type: "trace", trace_id: TRACE },
  deleted: false,
  created_at: "2026-01-01T10:11:00.000000000Z",
  updated_at: "2026-01-01T10:11:00.000000000Z",
  ingest_time: "2026-01-01T10:11:00.000000000Z",
};

function issueScore(issueId: string, index: number) {
  return {
    ...BASE,
    score_id: `01900000-0000-7000-8000-0000000003${index}0`,
    name: `issue:${issueId}`,
    value: { data_type: "numeric", value: 1 },
    source: "issue",
  };
}

function issue(id: string, title: string, state: string) {
  return {
    id,
    project_id: BASE.project_id,
    source_id: "01900000-0000-7000-8000-0000000000b1",
    source_name: "Errors",
    kind: "error",
    score_name: `issue:${id}`,
    title,
    description: null,
    state,
    regressed: false,
    merged_into: null,
    merged_from: [],
    first_seen: 1,
    last_seen: 2,
    count: 1,
    users: 1,
  };
}

/** Answers the trace's issue scores with `issueIds`, and the issues named with `issues`. */
function serveTraceIssues(issueIds: string[], issues: ReturnType<typeof issue>[]) {
  server.use(
    http.get("/api/v1/projects/:project/scores", ({ request }) => {
      const url = new URL(request.url);
      const data =
        url.searchParams.get("source") === "issue"
          ? issueIds.map((id, index) => issueScore(id, index))
          : [];
      return HttpResponse.json({ data, next_cursor: null });
    }),
    http.get("/api/v1/projects/:project/issues", ({ request }) => {
      const ids = new URL(request.url).searchParams.get("ids")?.split(",") ?? [];
      const data = issues.filter((item) => ids.includes(item.id));
      return HttpResponse.json({ data, next_cursor: null });
    }),
  );
}

afterEach(() => {
  server.resetHandlers();
});

describe("issue chips on a trace", () => {
  it("shows one chip per issue of the trace, linked to the issue page", async () => {
    serveTraceIssues(
      [OPEN, RESOLVED],
      [
        issue(OPEN, "Tool timeout", "open"),
        issue(RESOLVED, "Bad JSON from the model", "resolved"),
      ],
    );
    await renderApp(`/acme/support-bot/traces/${TRACE}`);
    const header = await screen.findByRole("region", { name: "Trace" });

    const open = await within(header).findByRole("link", { name: "Tool timeout" });
    expect(open).toHaveAttribute("href", `/acme/support-bot/issues/${OPEN}`);
    const resolved = within(header).getByRole("link", { name: "Bad JSON from the model" });
    expect(resolved).toHaveAttribute("href", `/acme/support-bot/issues/${RESOLVED}`);
  });

  it("cuts a title longer than 40 characters to 39 and an ellipsis", async () => {
    serveTraceIssues([LONG], [issue(LONG, LONG_TITLE, "open")]);
    await renderApp(`/acme/support-bot/traces/${TRACE}`);
    const header = await screen.findByRole("region", { name: "Trace" });

    const chip = await within(header).findByRole("link", { name: `${LONG_TITLE.slice(0, 39)}…` });
    expect(chip).toHaveAttribute("title", LONG_TITLE);
    expect(within(header).queryByText(LONG_TITLE)).not.toBeInTheDocument();
  });

  it("renders no chips when the trace has no issue scores", async () => {
    serveTraceIssues([], []);
    await renderApp(`/acme/support-bot/traces/${TRACE}`);
    const header = await screen.findByRole("region", { name: "Trace" });

    expect(within(header).queryByRole("link", { name: /issue/i })).not.toBeInTheDocument();
    expect(within(header).queryByRole("link", { name: "Tool timeout" })).not.toBeInTheDocument();
  });
});
