import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ScoreDefinitionsProvider } from "@/components/score-badge";
import { ProjectProvider } from "@/components/shell/project-context";
import { TraceSummary } from "@/components/trace/trace-summary";
import { TooltipProvider } from "@/components/ui/tooltip";
import { server } from "@/mocks/node";
import { serveAnnotationDefinitions } from "@/test/score-definitions";
import rootFixture from "../../../../fixtures/api/responses/spans/a1000000000000000000000000000001/b100000000000011.json";
import projectFixture from "../../../../fixtures/api/static/project.json";

const TRACE = "a1000000000000000000000000000001";
const ROOT = "b100000000000011";
const LLM = "b100000000000012";
const ERRORED = "b100000000000013";
const SPAN_URL = "/api/v1/projects/:project/traces/:trace_id/spans/:span_id";

function spanRow(
  spanId: string,
  name: string,
  overrides: Partial<Schema<"SpanSummary">> = {},
): Schema<"TreeSpan"> {
  return {
    depth: spanId === ROOT ? 0 : 1,
    detached: false,
    span: {
      span_id: spanId,
      trace_id: TRACE,
      span_kind: spanId === ROOT ? "agent" : "llm",
      name,
      start_time: "2026-01-01T10:00:00.000000000Z",
      end_time: "2026-01-01T10:00:01.000000000Z",
      duration_ns: 1_000_000_000,
      status: "ok",
      metadata: {},
      tags: [],
      ...(spanId === ROOT ? {} : { parent_span_id: ROOT }),
      ...overrides,
    },
  };
}

function spans(): Schema<"TreeSpan">[] {
  return [
    spanRow(ROOT, "support-agent"),
    spanRow(LLM, "chat claude-sonnet-4-5"),
    spanRow(ERRORED, "lookup_order", { status: "error", status_message: "Tool timed out" }),
  ];
}

function summary(overrides: Partial<Schema<"TraceSummary">> = {}): Schema<"TraceSummary"> {
  return {
    trace_id: TRACE,
    name: "support-agent",
    start_time: "2026-01-01T10:00:00.000000000Z",
    end_time: "2026-01-01T10:00:04.200000000Z",
    duration_ns: 4_200_000_000,
    status: "error",
    has_root: true,
    error_count: 1,
    llm_count: 2,
    span_count: 3,
    total_tokens: 2600,
    cost_total: 0.00306,
    metadata: {},
    models: [],
    tags: [],
    scores: [
      {
        name: "helpfulness",
        score_id: "01900000-0000-7000-8000-000000000701",
        source: "human",
        value: { data_type: "numeric", value: 0.9 },
      },
    ],
    ...overrides,
  };
}

function renderSummary(
  trace: Schema<"TraceSummary">,
  rows: Schema<"TreeSpan">[],
  onSelectSpan = vi.fn(),
) {
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
        <ScoreDefinitionsProvider>
          <TooltipProvider>
            <TraceSummary trace={trace} spans={rows} onSelectSpan={onSelectSpan} />
          </TooltipProvider>
        </ScoreDefinitionsProvider>
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onSelectSpan, ...view };
}

it("shows the trace totals as chips", async () => {
  serveAnnotationDefinitions();
  renderSummary(summary(), spans());
  const region = screen.getByRole("region", { name: "Trace summary" });

  expect(within(region).getByRole("heading", { name: "Trace summary" })).toBeInTheDocument();
  expect(within(region).getByText("4.20s")).toBeInTheDocument();
  expect(within(region).getByText("2.6k")).toBeInTheDocument();
  expect(within(region).getByText("$0.00306")).toBeInTheDocument();
  expect(within(region).getByText("3 spans")).toBeInTheDocument();
  expect(within(region).getByText("2 LLM calls")).toBeInTheDocument();
  expect(within(region).getByText("1 error")).toBeInTheDocument();
});

it("shows the scores, or says there are none", async () => {
  serveAnnotationDefinitions();
  const { unmount } = renderSummary(summary(), spans());
  const scores = within(screen.getByRole("region", { name: "Scores" }));
  expect(scores.getByText(/helpfulness/)).toHaveTextContent("helpfulness 0.9");
  expect(scores.queryByText("No scores")).not.toBeInTheDocument();
  unmount();

  renderSummary(summary({ scores: [] }), spans());
  expect(
    within(screen.getByRole("region", { name: "Scores" })).getByText("No scores"),
  ).toBeInTheDocument();
});

it("shows the question and the final answer from the root span", async () => {
  serveAnnotationDefinitions();
  server.use(
    http.get(SPAN_URL, ({ params }) => {
      if (params.span_id !== ROOT) return undefined;
      return HttpResponse.json({
        ...rootFixture,
        input: { format: "text", value: "What is the refund policy?" },
        output: { format: "text", value: "Refunds take five days." },
      });
    }),
  );
  renderSummary(summary(), spans());

  expect(await screen.findByText("What is the refund policy?")).toBeInTheDocument();
  expect(screen.getByText("Question")).toBeInTheDocument();
  expect(screen.getByText("Refunds take five days.")).toBeInTheDocument();
  expect(screen.getByText("Final answer")).toBeInTheDocument();
});

it("lists the erroring spans and selects one when its button is pressed", async () => {
  serveAnnotationDefinitions();
  const user = userEvent.setup();
  const { onSelectSpan } = renderSummary(summary(), spans());

  const errors = within(screen.getByRole("region", { name: "Errors" }));
  expect(errors.getByText("lookup_order")).toBeInTheDocument();
  expect(errors.getByText("Tool timed out")).toBeInTheDocument();
  await user.click(errors.getByRole("button", { name: /lookup_order/ }));
  expect(onSelectSpan).toHaveBeenCalledWith(ERRORED);
});

it("has no Errors section when no span errored", async () => {
  serveAnnotationDefinitions();
  renderSummary(summary({ error_count: 0 }), [
    spanRow(ROOT, "support-agent"),
    spanRow(LLM, "chat claude-sonnet-4-5"),
  ]);
  expect(screen.queryByRole("region", { name: "Errors" })).not.toBeInTheDocument();
});
