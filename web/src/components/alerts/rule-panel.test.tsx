import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import { projectRef } from "@/api/project-ref";
import { ProjectProvider } from "@/components/shell/project-context";
import { ThemeProvider } from "@/components/theme-provider";
import { server } from "@/mocks/node";
import { FIRING_RULE_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import issuePreviewFixture from "../../../../fixtures/api/static/alert-issue-preview.json";
import ruleFixture from "../../../../fixtures/api/static/alert-rule.json";
import issueFixture from "../../../../fixtures/api/static/issue.json";
import issueSourceFixture from "../../../../fixtures/api/static/issue-source.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { RulePanel } from "./rule-panel";

const ORG = "acme";
const PROJECT = "support-bot";
const RULE = "/api/v1/projects/:project/alert-rules/:id";
// Served before RULE: its `:id` also matches `issue-preview`.
const ISSUE_PREVIEW = "/api/v1/projects/:project/alert-rules/issue-preview";
const WIDGET = "/api/v1/projects/:project/dashboards/query";
const ISSUE = "/api/v1/projects/:project/issues/:id";
const SOURCE = "/api/v1/projects/:project/issue-sources/:id";
const ISSUE_ID = "01900000-0000-7000-8000-000000001c01";
const SOURCE_ID = "01900000-0000-7000-8000-000000001c00";

/** The rule fixture with an issue occurrences condition over the given scope. */
function scopedRule(scope: { issue_id: string } | { source_id: string }) {
  return {
    ...ruleFixture,
    condition: {
      metric: "issue_occurrences",
      window: "1h",
      comparator: "gt",
      threshold: 0,
      ...scope,
    },
  };
}

/** Renders the panel inside the project context, with a root route for its links. */
async function renderPanel(
  options: { ruleId?: string; onClose?: () => void; onDeleted?: () => void } = {},
) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  const onClose = options.onClose ?? vi.fn();
  const onDeleted = options.onDeleted ?? vi.fn();
  const rootRoute = createRootRoute({
    component: () => (
      <ProjectProvider
        value={{
          org: ORG,
          project: PROJECT,
          ref: projectRef(ORG, PROJECT),
          data: { ...projectFixture, role: "admin" },
        }}
      >
        <RulePanel
          ruleId={options.ruleId ?? FIRING_RULE_ID}
          layout={<span>Layout controls</span>}
          onClose={onClose}
          onDeleted={onDeleted}
        />
      </ProjectProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <RouterProvider router={router} />
      </ThemeProvider>
    </QueryClientProvider>,
  );
  await router.load();
  return { user: userEvent.setup(), onClose, onDeleted };
}

it("shows the rule's name, state and content", async () => {
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  await waitFor(() => {
    expect(within(panel).getByText(ruleFixture.name)).toBeInTheDocument();
  });
  const header = panel.querySelector("header") as HTMLElement;
  expect(within(header).getByText("Firing")).toBeInTheDocument();
  expect(within(header).getByText("Layout controls")).toBeInTheDocument();
  expect(await within(panel).findByText("Team Slack, Pager webhook")).toBeInTheDocument();
  expect(within(panel).getByText("Every 60 minutes while firing")).toBeInTheDocument();
  expect(within(panel).getByRole("table", { name: "Alert events" })).toBeInTheDocument();
});

it("has no Open page link; the list's layout controls open the page layout", async () => {
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  await waitFor(() => {
    expect(within(panel).getByText(ruleFixture.name)).toBeInTheDocument();
  });
  expect(within(panel).queryByRole("link", { name: "Open page" })).toBeNull();
});

it("shows the metric against its threshold above the history", async () => {
  server.use(
    http.post(WIDGET, () =>
      HttpResponse.json({
        shape: "series",
        bucket_ns: 300_000_000_000,
        series: [
          {
            measure: 0,
            other: false,
            points: [
              { time: "2026-01-01T00:00:00Z", value: 0.02, rows: 1 },
              { time: "2026-01-01T00:05:00Z", value: 0.08, rows: 1 },
            ],
          },
        ],
      }),
    ),
  );

  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  expect(
    await within(panel).findByRole("figure", {
      name: "Preview of the metric over the last 24 hours",
    }),
  ).toBeInTheDocument();
});

it("closes through the close button", async () => {
  const { user, onClose } = await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  await user.click(within(panel).getByRole("button", { name: "Close" }));

  expect(onClose).toHaveBeenCalledOnce();
});

it("says so when the rule no longer exists", async () => {
  server.use(http.get(RULE, () => problemResponse(404, "not_found")));

  await renderPanel();

  expect(await screen.findByText("This rule no longer exists.")).toBeInTheDocument();
});

it("calls onDeleted after the rule is deleted", async () => {
  server.use(http.delete(RULE, () => new HttpResponse(null, { status: 204 })));

  const { user, onDeleted } = await renderPanel();
  await user.click(await screen.findByRole("button", { name: "Delete rule" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete alert rule?" });
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  await waitFor(() => {
    expect(onDeleted).toHaveBeenCalledOnce();
  });
});

it("shows a channels error in place of the channel names", async () => {
  server.use(
    http.get("/api/v1/projects/:project/channels", () => problemResponse(500, "internal")),
  );

  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  expect(await within(panel).findByText("Internal Server Error")).toBeInTheDocument();
  expect(within(panel).queryByText("Team Slack, Pager webhook")).not.toBeInTheDocument();
  expect(within(panel).getByRole("table", { name: "Alert events" })).toBeInTheDocument();
});

it("shows a history error in place of the events table", async () => {
  server.use(
    http.get("/api/v1/projects/:project/alert-rules/:id/events", () =>
      problemResponse(500, "internal"),
    ),
  );

  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  expect(await within(panel).findByRole("alert")).toHaveTextContent("Internal Server Error");
  expect(within(panel).queryByRole("table", { name: "Alert events" })).not.toBeInTheDocument();
  expect(
    within(panel).queryByText("No events yet. Events appear when the rule fires or resolves."),
  ).not.toBeInTheDocument();
  expect(within(panel).getByText("Team Slack, Pager webhook")).toBeInTheDocument();
});

it("shows the title of the issue an issue rule counts", async () => {
  server.use(
    http.get(ISSUE_PREVIEW, () => HttpResponse.json(issuePreviewFixture)),
    http.get(RULE, () => HttpResponse.json(scopedRule({ issue_id: ISSUE_ID }))),
    http.get(ISSUE, () => HttpResponse.json(issueFixture)),
  );

  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  const link = await within(panel).findByRole("link", { name: issueFixture.title });
  expect(link).toHaveAttribute("href", `/acme/support-bot/issues/${ISSUE_ID}`);
});

it("shows the short id of the issue while it cannot be loaded", async () => {
  server.use(
    http.get(ISSUE_PREVIEW, () => HttpResponse.json(issuePreviewFixture)),
    http.get(RULE, () => HttpResponse.json(scopedRule({ issue_id: ISSUE_ID }))),
    http.get(ISSUE, () => problemResponse(500, "internal")),
  );

  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  expect(await within(panel).findByRole("link", { name: "01900000" })).toHaveAttribute(
    "href",
    `/acme/support-bot/issues/${ISSUE_ID}`,
  );
});

it("links a source rule to its source", async () => {
  server.use(
    http.get(ISSUE_PREVIEW, () => HttpResponse.json(issuePreviewFixture)),
    http.get(RULE, () => HttpResponse.json(scopedRule({ source_id: SOURCE_ID }))),
    http.get(SOURCE, () => HttpResponse.json(issueSourceFixture)),
  );

  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  const link = await within(panel).findByRole("link", { name: issueSourceFixture.name });
  expect(link).toHaveAttribute("href", `/acme/support-bot/issue-sources/${SOURCE_ID}`);
});
