import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { DataTable } from "@/components/data-table/data-table";
import { ProjectProvider } from "@/components/shell/project-context";
import {
  formatScore,
  orderedTraceColumns,
  traceColumnSpecs,
  traceRowTone,
} from "@/components/trace-columns";
import { TooltipProvider } from "@/components/ui/tooltip";
import projectFixture from "../../../fixtures/api/static/project.json";

it("marks only failed traces as an error row", () => {
  expect(traceRowTone({ status: "error" } as Schema<"TraceSummary">)).toBe("error");
  expect(traceRowTone({ status: "ok" } as Schema<"TraceSummary">)).toBeUndefined();
  expect(traceRowTone({ status: "unset" } as Schema<"TraceSummary">)).toBeUndefined();
});

it("formats each score type", () => {
  expect(formatScore({ data_type: "numeric", value: 0.9 })).toBe("0.90");
  expect(formatScore({ data_type: "boolean", value: false })).toBe("✗");
  expect(formatScore({ data_type: "categorical", value: { label: "correct", value: 1 } })).toBe(
    "correct",
  );
  expect(formatScore({ data_type: "text", value: "Needs retry" })).toBe("Needs retry");
});

function trace(overrides: Partial<Schema<"TraceSummary">> = {}): Schema<"TraceSummary"> {
  return {
    trace_id: "0af7651916cd43dd8448eb211c80319c",
    status: "error",
    name: "checkout",
    models: ["gpt-4o", "gpt-4o-mini"],
    session_id: "sess-1",
    user_id: "user-42",
    environment: "prod",
    scores: [],
    ...overrides,
  } as Schema<"TraceSummary">;
}

/** Renders the trace columns in a project route and a memory router, so "Open" items can navigate. */
async function renderTraces(
  rows: Schema<"TraceSummary">[],
  visible: string[],
  onFilter: (conjunct: string) => void,
) {
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
          <TooltipProvider>
            <DataTable
              tableId="traces-test"
              ariaLabel="Traces"
              columns={orderedTraceColumns(rows, visible, onFilter)}
              rows={rows}
              getRowId={(row) => row.trace_id}
              onActivate={() => undefined}
              hasNextPage={false}
              isFetchingNextPage={false}
              fetchNextPage={() => undefined}
              emptyState={null}
            />
          </TooltipProvider>
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
}

it("filters a model with contains from the right-click menu", async () => {
  const onFilter = vi.fn();
  await renderTraces([trace()], ["models"], onFilter);
  fireEvent.contextMenu(screen.getByText("gpt-4o +1"));
  fireEvent.click(await screen.findByRole("menuitem", { name: "Filter: models contains gpt-4o" }));
  expect(onFilter).toHaveBeenCalledWith("models @> ['gpt-4o']");
});

it("hides the prompts column by default", () => {
  expect(traceColumnSpecs([]).find((column) => column.id === "prompts")?.defaultVisible).toBe(
    undefined,
  );
});

it("shows the first prompt and a count of the rest", async () => {
  await renderTraces([trace({ prompts: ["a@1", "b@2"] })], ["prompts"], vi.fn());
  expect(screen.getByText("a@1 +1")).toBeInTheDocument();
});

it("filters the status with equals from the right-click menu", async () => {
  const onFilter = vi.fn();
  await renderTraces([trace()], ["status"], onFilter);
  fireEvent.contextMenu(screen.getByLabelText("Filter by status error"));
  fireEvent.click(await screen.findByRole("menuitem", { name: "Filter: status = 'error'" }));
  expect(onFilter).toHaveBeenCalledWith("status = 'error'");
});
