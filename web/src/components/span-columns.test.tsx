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
import { orderedSpanColumns, spanColumnSpecs } from "@/components/span-columns";
import { TooltipProvider } from "@/components/ui/tooltip";
import projectFixture from "../../../fixtures/api/static/project.json";

function span(overrides: Partial<Schema<"SpanSummary">> = {}): Schema<"SpanSummary"> {
  return {
    span_id: "00f067aa0ba902b7",
    trace_id: "0af7651916cd43dd8448eb211c80319c",
    name: "chat gpt-4o",
    span_kind: "llm",
    status: "error",
    model: "gpt-4o",
    provider: "openai",
    tags: [],
    ...overrides,
  } as Schema<"SpanSummary">;
}

async function renderSpans(
  rows: Schema<"SpanSummary">[],
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
              tableId="spans-test"
              ariaLabel="Spans"
              columns={orderedSpanColumns(visible, onFilter)}
              rows={rows}
              getRowId={(row) => row.span_id}
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

it("filters the kind with equals from the right-click menu", async () => {
  const onFilter = vi.fn();
  await renderSpans([span()], ["kind"], onFilter);
  fireEvent.contextMenu(screen.getByText("LLM"));
  fireEvent.click(await screen.findByRole("menuitem", { name: "Filter: span_kind = 'llm'" }));
  expect(onFilter).toHaveBeenCalledWith("span_kind = 'llm'");
});

it("filters the status with equals from the right-click menu", async () => {
  const onFilter = vi.fn();
  await renderSpans([span()], ["status"], onFilter);
  fireEvent.contextMenu(screen.getByLabelText("Filter by status error"));
  fireEvent.click(await screen.findByRole("menuitem", { name: "Filter: status = 'error'" }));
  expect(onFilter).toHaveBeenCalledWith("status = 'error'");
});

it("hides the prompt column by default", () => {
  expect(spanColumnSpecs().find((column) => column.id === "prompt")?.defaultVisible).toBe(
    undefined,
  );
});

it("shows the prompt name with its version", async () => {
  await renderSpans(
    [span({ prompt_name: "support-reply", prompt_version: 2 })],
    ["prompt"],
    vi.fn(),
  );
  expect(screen.getByText("support-reply@2")).toBeInTheDocument();
});

it("filters the provider with equals and excludes it from the menu", async () => {
  const onFilter = vi.fn();
  await renderSpans([span()], ["provider"], onFilter);
  fireEvent.contextMenu(screen.getByText("openai"));
  fireEvent.click(await screen.findByRole("menuitem", { name: "Exclude: provider != 'openai'" }));
  expect(onFilter).toHaveBeenCalledWith("provider != 'openai'");
});
