import { render, screen, within } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { SessionsSummary } from "./sessions-summary";

type Session = Schema<"SessionSummary">;

const MINUTE_NS = 60 * 1_000_000_000;

function session(overrides: Partial<Session>): Session {
  return {
    session_id: "sess",
    start_time: "2026-11-17T10:00:00.000Z",
    end_time: "2026-11-17T10:30:00.000Z",
    duration_ns: 30 * MINUTE_NS,
    trace_count: 1,
    error_count: 0,
    error_trace_count: 0,
    scores: [],
    tags: [],
    ...overrides,
  };
}

const ROWS: Session[] = [
  session({
    session_id: "a",
    trace_count: 2,
    error_trace_count: 1,
    duration_ns: 60 * MINUTE_NS,
    cost_total: 0.5,
    total_tokens: 500,
  }),
  session({
    session_id: "b",
    trace_count: 4,
    error_trace_count: 0,
    duration_ns: 30 * MINUTE_NS,
    cost_total: 1.5,
    total_tokens: 300,
  }),
];

function strip() {
  return screen.getByRole("region", { name: "Summary" });
}

it("computes the KPIs from the loaded rows", () => {
  render(<SessionsSummary sessions={ROWS} more={false} />);
  const summary = strip();
  expect(within(summary).getByText("Sessions")).toBeInTheDocument();
  expect(within(summary).getByText("2")).toBeInTheDocument();
  expect(within(summary).getByText("3")).toBeInTheDocument(); // avg turns
  expect(within(summary).getByText("45m 00s")).toBeInTheDocument(); // avg duration
  expect(within(summary).getByText("16.7%")).toBeInTheDocument(); // 1 of 6 traces
  expect(within(summary).getByText("$2.00")).toBeInTheDocument();
  expect(within(summary).getByText("800")).toBeInTheDocument();
  expect(within(summary).queryByText(/loaded/)).not.toBeInTheDocument();
});

it("adds a plus and an 'of N loaded' note when more pages exist", () => {
  render(<SessionsSummary sessions={ROWS} more />);
  const summary = strip();
  expect(within(summary).getByText("2+")).toBeInTheDocument();
  expect(within(summary).getByText("of 2 loaded")).toBeInTheDocument();
});

it("shows dashes for the KPIs with no rows or no values", () => {
  render(
    <SessionsSummary
      sessions={[session({ session_id: "c", trace_count: 0, duration_ns: 0 })]}
      more={false}
    />,
  );
  const summary = strip();
  expect(within(summary).getByText("1")).toBeInTheDocument();
  // Error rate has no traces to divide by, and cost and tokens are absent.
  expect(within(summary).getAllByText("—")).toHaveLength(3);
});

it("shows a zero count and dashes when no sessions are loaded", () => {
  render(<SessionsSummary sessions={[]} more={false} />);
  const summary = strip();
  expect(within(summary).getByText("0")).toBeInTheDocument();
  expect(within(summary).getAllByText("—")).toHaveLength(5);
});
