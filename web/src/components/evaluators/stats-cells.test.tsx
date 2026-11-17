import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import statsFixture from "../../../../fixtures/api/static/evaluator-stats.json";
import {
  AvgCostCell,
  ErrorsCell,
  errorRate,
  errorRateTone,
  LatencyP50Cell,
  ResultCell,
  RunsCell,
  resultText,
} from "./stats-cells";

type EvaluatorStats = Schema<"EvaluatorStats">;

const entries = statsFixture.data as EvaluatorStats[];
const relevance = entries.find((entry) => entry.evaluator_id.endsWith("0500")) as EvaluatorStats;
const answer = entries.find((entry) => entry.evaluator_id.endsWith("0501")) as EvaluatorStats;
const webhook = entries.find((entry) => entry.evaluator_id.endsWith("0502")) as EvaluatorStats;

describe("errorRate and errorRateTone", () => {
  it("alerts only above 5 %", () => {
    expect(errorRateTone(0.05)).toBe("text-muted-foreground");
    expect(errorRateTone(0.051)).toBe("text-destructive");
  });

  it("has no rate without a finished run", () => {
    expect(errorRate({ ...relevance.jobs, done: 0, failed: 0 })).toBeUndefined();
    expect(errorRate(relevance.jobs)).toBeCloseTo(0.075, 12);
  });
});

describe("stats cells", () => {
  it("shows the errors of the relevance evaluator in red", () => {
    render(<ErrorsCell stats={relevance} pending={false} />);
    const value = screen.getByText("7.5%");
    expect(value).toHaveClass("text-destructive");
    expect(value).toHaveAttribute("title", "3 failed of 40 runs");
  });

  it("shows the runs total and a sparkline of seven bars", () => {
    render(<RunsCell stats={relevance} pending={false} />);
    expect(screen.getByText("41")).toBeInTheDocument();
    const chart = screen.getByRole("img", { name: "Runs per day, last 7 days" });
    expect(chart.querySelectorAll("rect")).toHaveLength(7);
  });

  it("shows the average cost and the p50 latency, with the p95 as a title", () => {
    const { unmount } = render(<AvgCostCell stats={relevance} pending={false} />);
    expect(screen.getByText("$0.0016")).toBeInTheDocument();
    unmount();
    render(<LatencyP50Cell stats={relevance} pending={false} />);
    expect(screen.getByText("820ms")).toHaveAttribute("title", "p95 2.14s");
  });

  it("shows the result of each output type", () => {
    const { unmount } = render(<ResultCell stats={relevance} pending={false} />);
    expect(screen.getByText("0.82")).toBeInTheDocument();
    expect(screen.getByRole("img", { name: "Daily result, last 7 days" })).toBeInTheDocument();
    unmount();
    expect(resultText(answer.scores)).toBe("90% pass");
    const categorical = {
      ...answer.scores,
      data_type: "categorical" as const,
      top_category: { label: "pass", count: 12 },
    };
    expect(resultText(categorical)).toBe("pass (12)");
    const text = { ...answer.scores, data_type: "text" as const, count: 5 };
    expect(resultText(text)).toBe("5 scores");
    expect(resultText({ ...relevance.scores, count: 0 })).toBe("—");
  });

  it("shows a dash for an evaluator with no scores", () => {
    render(<ResultCell stats={webhook} pending={false} />);
    expect(screen.getByText("—")).toBeInTheDocument();
    expect(document.querySelector('[data-slot="skeleton"]')).toBeNull();
  });

  it("shows a skeleton while pending, and a dash once loaded without stats", () => {
    const { container, unmount } = render(<RunsCell stats={undefined} pending={true} />);
    expect(container.querySelector('[data-slot="skeleton"]')).not.toBeNull();
    unmount();
    const loaded = render(<RunsCell stats={undefined} pending={false} />);
    expect(within(loaded.container).getByText("—")).toBeInTheDocument();
    expect(loaded.container.querySelector('[data-slot="skeleton"]')).toBeNull();
  });
});
