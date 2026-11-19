import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  collectTrends,
  type ExperimentTrend,
  errorRatePoints,
  formatScore,
  scoreTrend,
  TrendLine,
} from "./dataset-trend";

const DATASET = "01900000-0000-7000-8000-000000000c01";

function experiment(
  id: string,
  name: string,
  scores: Schema<"ExperimentScoreMean">[] | undefined,
  results = { result_count: 10, error_count: 0 },
): ExperimentTrend {
  return {
    id,
    name,
    status: "completed",
    created_at: "2026-01-01T00:00:00.000000000Z",
    ...results,
    ...(scores === undefined ? {} : { scores }),
  };
}

const accuracy = (mean: number | undefined): Schema<"ExperimentScoreMean"> => ({
  name: "accuracy",
  data_type: "numeric",
  count: 10,
  ...(mean === undefined ? {} : { mean }),
});

const passed = (mean: number): Schema<"ExperimentScoreMean"> => ({
  name: "passed",
  data_type: "boolean",
  count: 10,
  mean,
});

it("shows the latest experiment's first numeric mean with two decimals", () => {
  const trend = scoreTrend([
    experiment("e2", "prompt v3", [passed(0.8), accuracy(0.873)]),
    experiment("e1", "prompt v2", [accuracy(0.5)]),
  ]);
  expect(trend?.name).toBe("accuracy");
  expect(trend?.latest).toBe("0.87");
});

it("falls back to the first boolean as a percentage when there is no numeric mean", () => {
  const trend = scoreTrend([
    experiment("e2", "prompt v3", [accuracy(undefined), passed(0.8)]),
    experiment("e1", "prompt v2", [accuracy(0.5)]),
  ]);
  expect(trend?.name).toBe("passed");
  expect(trend?.latest).toBe("80%");
});

it("plots the chosen score over the experiments, oldest first, skipping those without it", () => {
  const trend = scoreTrend([
    experiment("e3", "prompt v4", [accuracy(0.9)]),
    experiment("e2", "prompt v3", undefined),
    experiment("e1", "prompt v2", [accuracy(0.5)]),
  ]);
  expect(trend?.points.map((point) => point.id)).toEqual(["e1", "e3"]);
  expect(trend?.points.map((point) => point.value)).toEqual([0.5, 0.9]);
  expect(trend?.points[1]?.title).toBe("prompt v4: 0.90");
});

it("has no score without experiments, or when the latest has no measured score", () => {
  expect(scoreTrend([])).toBeUndefined();
  expect(scoreTrend([experiment("e1", "prompt v2", undefined)])).toBeUndefined();
  expect(scoreTrend([experiment("e1", "prompt v2", [accuracy(undefined)])])).toBeUndefined();
});

it("formats booleans as whole percentages and numbers with two decimals", () => {
  expect(formatScore("boolean", 0.5)).toBe("50%");
  expect(formatScore("numeric", 1)).toBe("1.00");
});

it("computes the error rate of experiments with results, oldest first", () => {
  const points = errorRatePoints([
    experiment("e2", "prompt v3", undefined, { result_count: 4, error_count: 1 }),
    experiment("e1", "prompt v2", undefined, { result_count: 0, error_count: 0 }),
    experiment("e0", "prompt v1", undefined, { result_count: 10, error_count: 0 }),
  ]);
  expect(points.map((point) => point.id)).toEqual(["e0", "e2"]);
  expect(points[1]?.value).toBe(0.25);
});

it("draws one dot per point, wrapped by the renderer when given", () => {
  render(
    <TrendLine
      label="accuracy over the last experiments"
      points={[
        { id: "e1", value: 0.5, title: "prompt v2: 0.50" },
        { id: "e2", value: 0.9, title: "prompt v3: 0.90" },
      ]}
      renderPoint={(point, dot) => <a href={`/experiments/${point.id}`}>{dot}</a>}
    />,
  );
  const chart = screen.getByRole("img", { name: "accuracy over the last experiments" });
  expect(chart.querySelectorAll("circle")).toHaveLength(2);
  expect(chart.querySelector("a[href='/experiments/e2']")).not.toBeNull();
});

it("marks a dataset without an answer as no experiments, and keeps still-loading rows out", () => {
  const trends = collectTrends([
    {
      ids: [DATASET, "other"],
      answer: {
        data: {
          data: [{ dataset_id: DATASET, experiments: [experiment("e1", "prompt v2", undefined)] }],
          scores_unavailable: true,
        },
        isError: false,
      },
    },
    { ids: ["loading"], answer: { isError: false } },
    { ids: ["failed"], answer: { isError: true } },
  ]);
  expect(trends.get(DATASET)).toMatchObject({ unavailable: true });
  expect(trends.get(DATASET)?.experiments).toHaveLength(1);
  expect(trends.get("other")).toEqual({ experiments: [], unavailable: true });
  expect(trends.has("loading")).toBe(false);
  expect(trends.get("failed")).toEqual({ experiments: [], unavailable: false });
});
