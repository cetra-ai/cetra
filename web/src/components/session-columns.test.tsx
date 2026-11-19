import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { DataTable } from "@/components/data-table/data-table";
import { ScoreDefinitionsProvider } from "@/components/score-badge";
import {
  orderedSessionColumns,
  sessionColumnSpecs,
  sessionScoreNames,
} from "@/components/session-columns";
import { ProjectProvider } from "@/components/shell/project-context";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SESSION_COLUMN_IDS } from "@/lib/search/sessions";
import projectFixture from "../../../fixtures/api/static/project.json";

const TWO_HOURS_AGO = new Date(Date.now() - 2 * 3_600_000).toISOString();

function session(overrides: Partial<Schema<"SessionSummary">> = {}): Schema<"SessionSummary"> {
  return {
    session_id: "sess-001",
    start_time: "2026-01-01T10:00:00.000000000Z",
    end_time: TWO_HOURS_AGO,
    duration_ns: 0,
    trace_count: 4,
    error_count: 1,
    error_trace_count: 1,
    total_tokens: 5150,
    cost_total: 0.00685,
    first_input_preview: "How do I reset my password?",
    last_output_preview: "Done, check your inbox.",
    scores: [],
    tags: [],
    ...overrides,
  };
}

function renderColumns(rows: Schema<"SessionSummary">[], visible: string[]) {
  const queryClient = new QueryClient();
  return render(
    <QueryClientProvider client={queryClient}>
      <ProjectProvider
        value={{
          org: "acme",
          project: "support-bot",
          ref: "acme/support-bot",
          data: projectFixture as Schema<"Project">,
        }}
      >
        <ScoreDefinitionsProvider>
          <TooltipProvider>
            <DataTable
              tableId="sessions-test"
              ariaLabel="Sessions"
              columns={orderedSessionColumns(rows, visible)}
              rows={rows}
              getRowId={(row) => row.session_id}
              onActivate={() => undefined}
              hasNextPage={false}
              isFetchingNextPage={false}
              fetchNextPage={() => undefined}
              emptyState={null}
            />
          </TooltipProvider>
        </ScoreDefinitionsProvider>
      </ProjectProvider>
    </QueryClientProvider>,
  );
}

function headerRow(name: string): HTMLElement {
  const header = screen
    .getByRole("grid", { name })
    .querySelector<HTMLElement>('[aria-rowindex="1"]');
  if (!header) {
    throw new Error("no header row");
  }
  return header;
}

it("shows the first message, error rate, last answer and relative last activity", () => {
  renderColumns([session()], ["first_message", "error_rate", "last_answer", "last"]);
  const grid = screen.getByRole("grid", { name: "Sessions" });
  expect(within(grid).getByText("How do I reset my password?")).toBeInTheDocument();
  expect(within(grid).getByText("Done, check your inbox.")).toBeInTheDocument();
  expect(within(grid).getByText("25%")).toHaveClass("text-destructive");
  const time = within(grid).getByText("2h ago");
  expect(time.tagName).toBe("TIME");
  expect(time.getAttribute("title")).toBeTruthy();
});

it("shows a dash for a missing preview and for an error rate without traces", () => {
  renderColumns(
    [session({ first_input_preview: undefined, trace_count: 0, error_trace_count: 0 })],
    ["first_message", "error_rate"],
  );
  const grid = screen.getByRole("grid", { name: "Sessions" });
  expect(within(grid).getAllByText("—")).toHaveLength(2);
});

it("draws inline bars on traces, tokens and cost", () => {
  renderColumns([session()], ["traces", "tokens", "cost"]);
  const grid = screen.getByRole("grid", { name: "Sessions" });
  expect(grid.querySelectorAll("[data-bar]")).toHaveLength(3);
});

it("gives the first message the remaining width, and keeps other columns fixed", () => {
  renderColumns([session()], ["session", "first_message", "traces"]);
  expect(headerRow("Sessions").style.gridTemplateColumns).toBe("160px minmax(360px, 1fr) 72px");
});

it("shows first message by default and keeps last answer hidden", () => {
  const specs = sessionColumnSpecs([]);
  expect(specs.find((spec) => spec.id === "first_message")?.defaultVisible).toBe(true);
  expect(specs.find((spec) => spec.id === "error_rate")?.defaultVisible).toBe(true);
  expect(specs.find((spec) => spec.id === "last_answer")?.defaultVisible).toBeUndefined();
  expect(specs.find((spec) => spec.id === "tags")?.defaultVisible).toBeUndefined();
});

it("orders the new columns and puts score columns after the fixed ones", () => {
  const ids = sessionColumnSpecs(["resolved"]).map((spec) => spec.id);
  expect(ids.slice(0, SESSION_COLUMN_IDS.length)).toEqual([...SESSION_COLUMN_IDS]);
  expect(ids.at(-1)).toBe("score:resolved");
  expect(sessionScoreNames([session({ scores: [] })])).toEqual([]);
});

it("filters the user from the right-click menu", async () => {
  const onFilter = vi.fn();
  const rows = [session({ user_id: "user-42" })];
  const queryClient = new QueryClient();
  const rootRoute = createRootRoute({
    component: () => (
      <QueryClientProvider client={queryClient}>
        <ProjectProvider
          value={{
            org: "acme",
            project: "support-bot",
            ref: "acme/support-bot",
            data: projectFixture as Schema<"Project">,
          }}
        >
          <ScoreDefinitionsProvider>
            <TooltipProvider>
              <DataTable
                tableId="sessions-test"
                ariaLabel="Sessions"
                columns={orderedSessionColumns(rows, ["user"], onFilter)}
                rows={rows}
                getRowId={(row) => row.session_id}
                onActivate={() => undefined}
                hasNextPage={false}
                isFetchingNextPage={false}
                fetchNextPage={() => undefined}
                emptyState={null}
              />
            </TooltipProvider>
          </ScoreDefinitionsProvider>
        </ProjectProvider>
      </QueryClientProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(<RouterProvider router={router} />);
  await router.load();
  fireEvent.contextMenu(screen.getByText("user-42"));
  fireEvent.click(await screen.findByRole("menuitem", { name: "Filter: user_id = 'user-42'" }));
  expect(onFilter).toHaveBeenCalledWith("user_id = 'user-42'");
});
