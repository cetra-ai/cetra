import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { liveScoreDefinitionsQuery } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { DataTable } from "@/components/data-table/data-table";
import { ScoreAverageBadge, ScoreBadge, ScoreDefinitionsProvider } from "@/components/score-badge";
import { ProjectProvider } from "@/components/shell/project-context";
import { orderedTraceColumns } from "@/components/trace-columns";
import { TooltipProvider } from "@/components/ui/tooltip";
import { server } from "@/mocks/node";
import projectFixture from "../../../fixtures/api/static/project.json";

function definition(overrides: Partial<Schema<"ScoreDefinition">>): Schema<"ScoreDefinition"> {
  return {
    id: "01900000-0000-7000-8000-000000000601",
    name: "helpfulness",
    data_type: "numeric",
    direction: "higher",
    categories: [],
    min_value: 0,
    max_value: 1,
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
    ...overrides,
  };
}

function serve(definitions: Schema<"ScoreDefinition">[]) {
  server.use(
    http.get("/api/v1/projects/:project/score-definitions", () =>
      HttpResponse.json({ data: definitions, next_cursor: null }),
    ),
  );
}

/** Renders the badge and waits until the project's definitions have loaded into the cache. */
async function renderBadge(node: React.ReactNode) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  const view = render(
    <QueryClientProvider client={queryClient}>
      <ProjectProvider
        value={{
          org: "acme",
          project: "support-bot",
          ref: "acme/support-bot",
          data: projectFixture as Schema<"Project">,
        }}
      >
        <ScoreDefinitionsProvider>{node}</ScoreDefinitionsProvider>
      </ProjectProvider>
    </QueryClientProvider>,
  );
  const key = liveScoreDefinitionsQuery("acme", "support-bot").queryKey;
  await waitFor(() => {
    expect(queryClient.getQueryData(key)).toBeDefined();
  });
  return view;
}

const GOOD = { data_type: "numeric", value: 0.9 } as const;
const BAD = { data_type: "numeric", value: 0.1 } as const;

it("colours a value good when its definition is higher-is-better", async () => {
  serve([definition({})]);
  await renderBadge(<ScoreBadge name="helpfulness" value={GOOD} text="0.90" />);
  expect(screen.getByText("0.90")).toHaveClass("tone-good", "px-1");
});

it("colours a low value bad", async () => {
  serve([definition({})]);
  await renderBadge(<ScoreBadge name="helpfulness" value={BAD} text="0.10" />);
  expect(screen.getByText("0.10")).toHaveClass("tone-bad");
});

it("shows plain text when there is no definition or it has no direction", async () => {
  serve([definition({ direction: "none" })]);
  await renderBadge(<ScoreBadge name="helpfulness" value={GOOD} text="0.90" className="num" />);
  expect(screen.getByText("0.90")).toHaveClass("num");
  expect(screen.getByText("0.90")).not.toHaveClass("rounded-sm");
});

it("shows plain text when no definition exists", async () => {
  serve([]);
  await renderBadge(<ScoreBadge name="helpfulness" value={GOOD} text="0.90" />);
  expect(screen.getByText("0.90")).not.toHaveClass("rounded-sm");
});

it("colours a user's boolean average from the share of true", async () => {
  serve([
    definition({
      name: "thumbs",
      data_type: "boolean",
      min_value: undefined,
      max_value: undefined,
    }),
  ]);
  await renderBadge(<ScoreAverageBadge name="thumbs" avg={0.2} dataType="boolean" text="20%" />);
  expect(screen.getByText("20%")).toHaveClass("tone-bad");
});

function traceRow(value: Schema<"ScoreValue">): Schema<"TraceSummary"> {
  return {
    trace_id: "a1000000000000000000000000000001",
    scores: [
      {
        score_id: "01900000-0000-7000-8000-000000000900",
        name: "helpfulness",
        source: "api",
        value,
      },
    ],
  } as unknown as Schema<"TraceSummary">;
}

/** Renders the trace list's score column for one trace. */
async function renderTraceScore(value: Schema<"ScoreValue">) {
  const row = traceRow(value);
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  render(
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
              tableId="traces-score-test"
              ariaLabel="Traces"
              columns={orderedTraceColumns([row], ["score:helpfulness"])}
              rows={[row]}
              getRowId={(item) => item.trace_id}
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
  const key = liveScoreDefinitionsQuery("acme", "support-bot").queryKey;
  await waitFor(() => {
    expect(queryClient.getQueryData(key)).toBeDefined();
  });
}

it("colours a trace list score cell by its definition", async () => {
  serve([definition({})]);
  await renderTraceScore({ data_type: "numeric", value: 0.9 });
  expect(screen.getByText("0.90")).toHaveClass("tone-good");
});

it("colours a low trace list score cell bad", async () => {
  serve([definition({})]);
  await renderTraceScore({ data_type: "numeric", value: 0.1 });
  expect(screen.getByText("0.10")).toHaveClass("tone-bad");
});

it("shows a trace list score cell without a pill when there is no definition", async () => {
  serve([]);
  await renderTraceScore({ data_type: "numeric", value: 0.9 });
  expect(screen.getByText("0.90")).not.toHaveClass("rounded-sm");
});
