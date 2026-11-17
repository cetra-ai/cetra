import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ScoreDefinitionsProvider } from "@/components/score-badge";
import { ProjectProvider } from "@/components/shell/project-context";
import { TooltipProvider } from "@/components/ui/tooltip";
import { PlanPill, UserChips, UserMetadata, UserScores } from "@/components/user/user-chips";
import projectFixture from "../../../../fixtures/api/static/project.json";

type User = Schema<"EndUserSummary">;

function user(overrides: Partial<User> = {}): User {
  return {
    user_id: "user-42",
    activity: [],
    error_count: 0,
    error_trace_count: 0,
    first_seen: "2026-11-01T00:00:00.000Z",
    last_seen: "2026-11-02T00:00:00.000Z",
    scores: [],
    session_count: 2,
    trace_count: 4,
    ...overrides,
  };
}

/** Score badges read the project's score definitions, so they need the providers. */
function ScoresWithProviders({ scores }: { scores: Schema<"UserScore">[] }) {
  return (
    <QueryClientProvider client={new QueryClient()}>
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
            <UserScores scores={scores} />
          </TooltipProvider>
        </ScoreDefinitionsProvider>
      </ProjectProvider>
    </QueryClientProvider>
  );
}

it("shows the totals as chips and an error rate without errors in the plain colour", () => {
  const { container } = render(<UserChips user={user({ total_tokens: 1500, cost_total: 0.25 })} />);
  expect(screen.getByText("4 traces")).toBeInTheDocument();
  expect(screen.getByText("2 sessions")).toBeInTheDocument();
  expect(screen.getByText("0%")).toBeInTheDocument();
  expect(container.innerHTML).not.toContain("text-status-error");
});

it("marks the error rate when a trace failed and singularises one trace", () => {
  render(<UserChips user={user({ trace_count: 1, session_count: 1, error_trace_count: 1 })} />);
  expect(screen.getByText("1 trace")).toBeInTheDocument();
  expect(screen.getByText("100%").parentElement).toHaveClass("text-status-error");
});

it("leaves out the tokens and cost chips when they are undefined", () => {
  render(<UserChips user={user()} />);
  expect(screen.queryByText("Tokens")).not.toBeInTheDocument();
  expect(screen.queryByText("Cost")).not.toBeInTheDocument();
});

it("shows the plan", () => {
  render(<PlanPill plan="team" />);
  expect(screen.getByText("team")).toBeInTheDocument();
});

it("lists the score averages and says when there are none", () => {
  const { unmount } = render(<ScoresWithProviders scores={[]} />);
  expect(screen.getByText("No scores")).toBeInTheDocument();
  unmount();
  render(
    <ScoresWithProviders
      scores={[{ name: "resolved", data_type: "boolean", avg: 0.5, count: 3 }]}
    />,
  );
  expect(screen.getByText("resolved")).toBeInTheDocument();
  expect(screen.getByText("50%")).toBeInTheDocument();
});

it("lists the metadata decoded and says when there is none", () => {
  const { unmount } = render(<UserMetadata metadata={undefined} />);
  expect(screen.getByText("No metadata")).toBeInTheDocument();
  unmount();
  render(<UserMetadata metadata={{ plan: '"team"' }} />);
  expect(screen.getByText("plan")).toBeInTheDocument();
  expect(screen.getByText("team")).toBeInTheDocument();
});
