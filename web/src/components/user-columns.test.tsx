import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, within } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { DataTable } from "@/components/data-table/data-table";
import { ScoreDefinitionsProvider } from "@/components/score-badge";
import { ProjectProvider } from "@/components/shell/project-context";
import { TooltipProvider } from "@/components/ui/tooltip";
import { orderedUserColumns, userColumnSpecs, userScoreNames } from "@/components/user-columns";
import { USER_COLUMN_IDS } from "@/lib/search/users";
import projectFixture from "../../../fixtures/api/static/project.json";

const ACTIVITY = Array.from({ length: 24 }, (_, index) => (index === 3 ? 4 : 0));

function summary(overrides: Partial<Schema<"EndUserSummary">> = {}): Schema<"EndUserSummary"> {
  return {
    user_id: "user-42",
    first_seen: "2026-01-01T10:00:00.000000000Z",
    last_seen: "2026-01-01T10:20:03.000000000Z",
    trace_count: 4,
    session_count: 2,
    error_count: 1,
    error_trace_count: 1,
    total_tokens: 5150,
    cost_total: 0.00685,
    activity: ACTIVITY,
    scores: [
      { name: "thumbs", data_type: "boolean", avg: 0.75, count: 4 },
      { name: "helpfulness", data_type: "numeric", avg: 0.7, count: 2 },
    ],
    metadata: { name: '"Ada Example"', plan: '"pro"' },
    ...overrides,
  };
}

function renderColumns(rows: Schema<"EndUserSummary">[], visible: string[]) {
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
              tableId="users-test"
              ariaLabel="Users"
              columns={orderedUserColumns(rows, visible)}
              rows={rows}
              getRowId={(row) => row.user_id}
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

it("shows error rate, traces per session, score rates, activity and metadata", () => {
  renderColumns(
    [summary()],
    [
      "user",
      "activity",
      "traces_per_session",
      "error_rate",
      "score:thumbs",
      "score:helpfulness",
      "metadata",
    ],
  );
  const grid = screen.getByRole("grid", { name: "Users" });
  expect(within(grid).getByText("25%")).toHaveClass("text-destructive");
  expect(within(grid).getByText("2.0")).toBeInTheDocument();
  expect(within(grid).getByText("75%")).toBeInTheDocument();
  expect(within(grid).getByText("0.70")).toBeInTheDocument();
  expect(within(grid).getByRole("img", { name: "4 traces over the range" })).toBeInTheDocument();
  expect(within(grid).getByText("name: Ada Example · plan: pro")).toBeInTheDocument();
});

it("shows the name with the id after it and the plan pill (UP-4)", () => {
  renderColumns([summary(), summary({ user_id: "user-7", metadata: undefined })], ["user"]);
  const grid = screen.getByRole("grid", { name: "Users" });
  expect(within(grid).getByText("Ada Example")).toBeInTheDocument();
  expect(within(grid).getByText("user-42")).toHaveClass("font-mono");
  expect(within(grid).getByText("pro")).toBeInTheDocument();
  expect(within(grid).getByTitle("Ada Example (user-42)")).toBeInTheDocument();
  expect(within(grid).getByText("user-7")).toHaveClass("font-mono");
  expect(within(grid).getByTitle("user-7")).toBeInTheDocument();
});

it("shows first and last seen as relative time", () => {
  vi.useFakeTimers({ now: Date.parse("2026-01-01T12:00:00.000Z") });
  try {
    renderColumns([summary()], ["first_seen", "last_seen"]);
  } finally {
    vi.useRealTimers();
  }
  const grid = screen.getByRole("grid", { name: "Users" });
  expect(within(grid).getByText("2h ago")).toBeInTheDocument();
  expect(within(grid).getByText("1h ago")).toBeInTheDocument();
});

it("shows a dash for error rate and traces per session without traces or sessions", () => {
  renderColumns(
    [summary({ trace_count: 0, session_count: 0, error_trace_count: 0, scores: [] })],
    ["error_rate", "traces_per_session"],
  );
  const grid = screen.getByRole("grid", { name: "Users" });
  expect(within(grid).getAllByText("—")).toHaveLength(2);
});

it("collects score names from the loaded rows, sorted", () => {
  const rows = [
    summary({ scores: [{ name: "thumbs", data_type: "boolean", avg: 1, count: 1 }] }),
    summary({
      user_id: "user-7",
      scores: [{ name: "helpfulness", data_type: "numeric", avg: 0.5, count: 1 }],
    }),
  ];
  expect(userScoreNames(rows)).toEqual(["helpfulness", "thumbs"]);
});

it("makes exactly the server-sorted columns clickable", () => {
  renderColumns([summary()], [...USER_COLUMN_IDS]);
  const grid = screen.getByRole("grid", { name: "Users" });
  const sortable = within(grid)
    .getAllByRole("columnheader")
    .filter((header) => header.hasAttribute("aria-sort"))
    .map((header) => header.textContent);
  expect(sortable.sort()).toEqual(
    [
      "Traces",
      "Sessions",
      "Traces / session",
      "Tokens",
      "Cost",
      "Errors",
      "Error rate",
      "First seen",
      "Last seen",
    ].sort(),
  );
});

it("puts one score column after error rate and hides metadata by default", () => {
  const specs = userColumnSpecs(["helpfulness", "thumbs"]);
  const ids = specs.map((spec) => spec.id);
  const at = ids.indexOf("error_rate");
  expect(ids.slice(at, at + 3)).toEqual(["error_rate", "score:helpfulness", "score:thumbs"]);
  expect(specs.find((spec) => spec.id === "metadata")?.defaultVisible).toBeUndefined();
  expect(specs.find((spec) => spec.id === "activity")).toMatchObject({
    label: "Activity",
    defaultVisible: true,
  });
});
