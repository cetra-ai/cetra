import { render, screen, within } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ScoreDistribution } from "./score-distribution";

function summary(partial: Partial<Schema<"ScoreSummary">>): Schema<"ScoreSummary"> {
  return { name: "score", data_type: "numeric", count: 0, labels: {}, histogram: [], ...partial };
}

it("draws a bar per bucket of a numeric score, with its range", () => {
  const histogram = [1, 0, 2, 0, 0, 0, 0, 0, 0, 4];
  render(
    <ScoreDistribution
      summary={summary({ name: "quality", count: 7, histogram, min: 0, max: 1 })}
    />,
  );

  const chart = screen.getByRole("img", { name: "Distribution of quality" });
  expect(chart.children).toHaveLength(10);
  expect(chart.children[0]).toHaveAttribute("title", "0.000–0.100: 1");
  expect(chart.children[9]).toHaveAttribute("title", "0.900–1.000: 4");
});

it("draws a single bucket when every value is equal", () => {
  render(
    <ScoreDistribution
      summary={summary({ name: "quality", count: 3, histogram: [3], min: 0.5, max: 0.5 })}
    />,
  );

  const chart = screen.getByRole("img", { name: "Distribution of quality" });
  expect(chart.children).toHaveLength(1);
  expect(chart.children[0]).toHaveAttribute("title", "0.500–0.500: 3");
});

it("splits a boolean score into pass and fail", () => {
  render(
    <ScoreDistribution
      summary={summary({ name: "exact", data_type: "boolean", count: 4, pass_rate: 0.75 })}
    />,
  );

  expect(screen.getByRole("img", { name: "Pass 75 %, fail 25 %" })).toBeInTheDocument();
  expect(screen.getByText("pass 75 % · fail 25 %")).toBeInTheDocument();
});

it("stacks the top labels of a categorical score and folds the rest into other", () => {
  render(
    <ScoreDistribution
      summary={summary({
        name: "tone",
        data_type: "categorical",
        count: 12,
        labels: { warm: 5, neutral: 3, cold: 2, angry: 1, rude: 1 },
      })}
    />,
  );

  const chart = screen.getByRole("img", { name: "Labels of tone" });
  expect(chart.children).toHaveLength(5);
  const legend = screen.getByRole("list");
  expect(within(legend).getByText("warm 5")).toBeInTheDocument();
  expect(within(legend).getByText("other 1")).toBeInTheDocument();
});

it("draws nothing for a text score", () => {
  const { container } = render(
    <ScoreDistribution summary={summary({ name: "notes", data_type: "text", count: 2 })} />,
  );

  expect(container).toBeEmptyDOMElement();
});
