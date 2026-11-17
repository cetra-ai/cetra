import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ScoreSummary, scoreText, scoreValue } from "./score-summary";

type RuleScores = Schema<"RuleScoreStats">;

const POINTS = Array.from({ length: 24 }, (_, index) => ({
  time: `2026-01-01T${String(index).padStart(2, "0")}:00:00.000000000Z`,
}));

function scores(overrides: Partial<RuleScores>): RuleScores {
  return { data_type: "numeric", count: 0, hourly: POINTS, ...overrides };
}

describe("score text and value", () => {
  it("shows the average, the pass rate, the top label or the count", () => {
    expect(scoreText(scores({ count: 4, average: 0.8345 }))).toBe("0.83");
    expect(scoreText(scores({ data_type: "boolean", count: 4, pass_rate: 0.75 }))).toBe("75 %");
    expect(
      scoreText(
        scores({
          data_type: "categorical",
          count: 4,
          top_category: { label: "good", count: 3 },
        }),
      ),
    ).toBe("good (3)");
    expect(scoreText(scores({ data_type: "text", count: 2 }))).toBe("2 scores");
  });

  it("says no scores when there are none", () => {
    expect(scoreText(scores({ count: 0 }))).toBe("No scores");
  });

  it("sorts by the average or the pass rate only", () => {
    expect(scoreValue(scores({ count: 4, average: 0.5 }))).toBe(0.5);
    expect(scoreValue(scores({ data_type: "boolean", count: 4, pass_rate: 0.25 }))).toBe(0.25);
    expect(scoreValue(scores({ data_type: "text", count: 2 }))).toBeUndefined();
    expect(scoreValue(undefined)).toBeUndefined();
  });
});

describe("ScoreSummary", () => {
  it("draws the trend line for a numeric rule", () => {
    render(<ScoreSummary scores={scores({ count: 4, average: 0.8 })} pending={false} />);

    expect(screen.getByText("0.80")).toBeInTheDocument();
    expect(
      screen.getByRole("img", { name: "Average score per hour, last 24 h" }),
    ).toBeInTheDocument();
    expect(screen.getByText("4 scores · 24 h")).toBeInTheDocument();
  });

  it("shows No scores without a trend line", () => {
    render(<ScoreSummary scores={scores({ count: 0 })} pending={false} />);

    expect(screen.getByText("No scores")).toBeInTheDocument();
    expect(screen.queryByRole("img")).toBeNull();
  });

  it("shows a dash when the rule has no stats", () => {
    render(<ScoreSummary scores={undefined} pending={false} />);

    expect(screen.getByText("—")).toBeInTheDocument();
  });
});
