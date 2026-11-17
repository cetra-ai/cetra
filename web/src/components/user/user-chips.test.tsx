import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
  expect(screen.getByText("100%").parentElement).toHaveClass("bg-status-error/10");
});

it("tints the sessions chip with chart-1 and leaves the traces chip neutral", () => {
  render(<UserChips user={user({ trace_count: 1, session_count: 1 })} />);
  expect(screen.getByText("1 session").parentElement).toHaveClass("[&>svg]:text-[var(--chart-1)]");
  expect(screen.getByText("1 trace").parentElement).not.toHaveClass(
    "[&>svg]:text-[var(--chart-1)]",
  );
});

it("mutes the sessions count when the user has none here but the range has some", () => {
  render(
    <TooltipProvider>
      <UserChips user={user({ session_count: 0 })} totalSessions={3} />
    </TooltipProvider>,
  );
  expect(screen.getByText("0 sessions").parentElement).toHaveClass(
    "[&_.num]:text-muted-foreground",
  );
});

it("explains the sessions chip with the range and the total when the total is known", async () => {
  render(
    <TooltipProvider>
      <UserChips user={user({ session_count: 29 })} totalSessions={31} />
    </TooltipProvider>,
  );
  const userEventSetup = userEvent.setup();
  // The first pointer enter remounts the chip, so each step queries it again.
  await userEventSetup.hover(screen.getByText("29 sessions"));
  await userEventSetup.unhover(screen.getByText("29 sessions"));
  await userEventSetup.hover(screen.getByText("29 sessions"));
  const tooltip = await screen.findByRole("tooltip");
  expect(tooltip).toHaveTextContent("29 sessions in this range");
  expect(tooltip).toHaveTextContent("31 sessions in total");
});

it("has no tooltip on the sessions chip without a total", async () => {
  render(
    <TooltipProvider>
      <UserChips user={user({ session_count: 29 })} />
    </TooltipProvider>,
  );
  const chip = screen.getByText("29 sessions");
  const userEventSetup = userEvent.setup();
  await userEventSetup.hover(chip);
  await userEventSetup.unhover(chip);
  await userEventSetup.hover(chip);
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
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

it("fills a boolean score bar by its share of true, and draws no fill for a score without a direction", () => {
  const { unmount } = render(
    <ScoresWithProviders
      scores={[{ name: "resolved", data_type: "boolean", avg: 0.5, count: 3 }]}
    />,
  );
  expect(screen.getByTestId("score-fill")).toHaveStyle({ width: "50%" });
  unmount();
  render(
    <ScoresWithProviders
      scores={[{ name: "zz-unknown-score", data_type: "numeric", avg: 3, count: 2 }]}
    />,
  );
  expect(screen.queryByTestId("score-fill")).not.toBeInTheDocument();
});

it("lists the metadata decoded and says when there is none", () => {
  const { unmount } = render(<UserMetadata metadata={undefined} />);
  expect(screen.getByText("No metadata")).toBeInTheDocument();
  unmount();
  render(<UserMetadata metadata={{ plan: '"team"' }} />);
  expect(screen.getByText("plan")).toBeInTheDocument();
  expect(screen.getByText("team")).toBeInTheDocument();
});

it("shows a two-letter country with its flag hidden from screen readers", () => {
  render(<UserMetadata metadata={{ country: '"DE"' }} />);
  expect(screen.getByText("DE")).toBeInTheDocument();
  expect(screen.getByText("🇩🇪")).toHaveAttribute("aria-hidden", "true");
});

it("gives a long metadata value its full text as title", () => {
  const long = "a very long note that will not fit in the pill ".repeat(3).trim();
  render(<UserMetadata metadata={{ note: JSON.stringify(long) }} />);
  expect(screen.getByText(long)).toHaveAttribute("title", long);
});
