import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { ThemeProvider } from "@/components/theme-provider";
import { server } from "@/mocks/node";
import eventsFixture from "../../../../fixtures/api/static/eval-jobs.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { failureGroups, RecentFailures } from "./recent-failures";

const ORG = "acme";
const PROJECT = "support-bot";
const RULE_ID = "01900000-0000-7000-8000-000000000700";
const TRACE_A = "a1000000000000000000000000000002";
const TRACE_B = "a1000000000000000000000000000003";
const TRACE_C = "a1000000000000000000000000000004";

type Job = Schema<"EvalJob">;

/** A failed job from the fixture, with its own id, trace, time and error. */
function failedJob(overrides: Partial<Job>): Job {
  return { ...eventsFixture.data[1], ...overrides } as Job;
}

const JOBS: Job[] = [
  failedJob({
    id: "01900000-0000-7000-8000-000000000811",
    trace_id: TRACE_A,
    span_id: undefined,
    error: "provider status 503",
    error_class: "retryable",
    finished_at: "2026-01-01T10:05:00.000000000Z",
  }),
  failedJob({
    id: "01900000-0000-7000-8000-000000000812",
    trace_id: TRACE_B,
    span_id: "b100000000000001",
    error: "provider status 503",
    error_class: "retryable",
    finished_at: "2026-01-01T10:09:00.000000000Z",
  }),
  failedJob({
    id: "01900000-0000-7000-8000-000000000813",
    trace_id: TRACE_C,
    error: "context length exceeded",
    error_class: "permanent",
    finished_at: "2026-01-01T10:07:00.000000000Z",
  }),
];

/** The section needs the router for its links and the project context for the org and project. */
async function renderSection() {
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
        <RecentFailures org={ORG} project={PROJECT} ruleId={RULE_ID} />
      </ProjectProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <QueryClientProvider
      client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
    >
      <ThemeProvider>
        <RouterProvider router={router} />
      </ThemeProvider>
    </QueryClientProvider>,
  );
  await router.load();
}

function jobsResponse(data: Job[]) {
  return http.get("/api/v1/projects/:project/eval-jobs", () =>
    HttpResponse.json({ data, next_cursor: null }),
  );
}

it("groups the failures by error and lists the most frequent first", async () => {
  let query: URLSearchParams | undefined;
  server.use(
    http.get("/api/v1/projects/:project/eval-jobs", ({ request }) => {
      query = new URL(request.url).searchParams;
      return HttpResponse.json({ data: JOBS, next_cursor: null });
    }),
  );
  await renderSection();

  const section = await screen.findByRole("region", { name: "Recent failures" });
  const rows = within(section).getAllByRole("listitem");
  expect(rows).toHaveLength(2);
  expect(rows[0]).toHaveTextContent("2×");
  expect(rows[0]).toHaveTextContent("provider status 503");
  expect(rows[0]).toHaveTextContent("retryable");
  expect(rows[1]).toHaveTextContent("1×");
  expect(rows[1]).toHaveTextContent("context length exceeded");
  expect(rows[1]).toHaveTextContent("permanent");

  // The link goes to the newest job of the group, with its span when it has one.
  const links = within(rows[0] as HTMLElement).getByRole("link", { name: "View trace" });
  expect(links).toHaveAttribute(
    "href",
    `/acme/support-bot/traces/${TRACE_B}?span=b100000000000001`,
  );
  expect(within(rows[1] as HTMLElement).getByRole("link", { name: "View trace" })).toHaveAttribute(
    "href",
    `/acme/support-bot/traces/${TRACE_C}`,
  );

  expect(query?.get("rule_id")).toBe(RULE_ID);
  expect(query?.get("status")).toBe("failed");
});

it("shows nothing when the rule has no failed jobs", async () => {
  server.use(jobsResponse([{ ...eventsFixture.data[0], status: "done" } as Job]));
  await renderSection();

  await waitFor(() => expect(document.querySelector('[data-slot="skeleton"]')).toBeNull());
  expect(screen.queryByRole("region", { name: "Recent failures" })).toBeNull();
});

it("shows an alert when the failures do not load", async () => {
  server.use(
    http.get("/api/v1/projects/:project/eval-jobs", () =>
      HttpResponse.json({ type: "about:blank", title: "internal", status: 500 }, { status: 500 }),
    ),
  );
  await renderSection();

  const section = await screen.findByRole("region", { name: "Recent failures" });
  expect(await within(section).findByRole("alert")).toHaveTextContent(
    "Could not load the failures.",
  );
});

it("orders groups with the same count by the newest failure and names a missing message", () => {
  const groups = failureGroups([
    failedJob({
      id: "01900000-0000-7000-8000-000000000821",
      error: "old",
      error_class: "permanent",
      finished_at: "2026-01-01T09:00:00.000000000Z",
    }),
    failedJob({
      id: "01900000-0000-7000-8000-000000000822",
      error: "new",
      error_class: "permanent",
      finished_at: "2026-01-01T11:00:00.000000000Z",
    }),
    failedJob({
      id: "01900000-0000-7000-8000-000000000823",
      error: undefined,
      error_class: "retryable",
      finished_at: "2026-01-01T10:00:00.000000000Z",
    }),
    { ...eventsFixture.data[0], status: "done" } as Job,
  ]);

  expect(groups.map((group) => group.error)).toEqual(["new", undefined, "old"]);
  expect(groups.map((group) => group.count)).toEqual([1, 1, 1]);
});
