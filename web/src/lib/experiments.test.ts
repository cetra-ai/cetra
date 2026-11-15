import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  compareBlockReason,
  deltaDirection,
  deltaText,
  expectedRuns,
  parseCompareSearch,
  parseExperimentSearch,
  parseExperimentsSearch,
  scoreSummaryText,
  scoreValueText,
} from "./experiments";

const A = "01900000-0000-7000-8000-00000000ea01";
const B = "01900000-0000-7000-8000-00000000eb02";
const C = "01900000-0000-7000-8000-00000000ec03";
const D = "01900000-0000-7000-8000-00000000ed04";
const E = "01900000-0000-7000-8000-00000000ee05";

function summary(partial: Partial<Schema<"ScoreSummary">>): Schema<"ScoreSummary"> {
  return { name: "score", data_type: "numeric", count: 40, labels: {}, ...partial };
}

describe("parseCompareSearch", () => {
  it("keeps the first four distinct UUIDs in order", () => {
    const parsed = parseCompareSearch({ ids: `${A}, ${B},,${A},x,${C},${D},${E}` });
    expect(parsed.ids).toEqual([A, B, C, D]);
  });

  it("reads the threshold within [0, 1000] only", () => {
    expect(parseCompareSearch({ ids: A, threshold: "0.2" }).threshold).toBe(0.2);
    expect(parseCompareSearch({ ids: A, threshold: "-1" }).threshold).toBeUndefined();
    expect(parseCompareSearch({ ids: A, threshold: "abc" }).threshold).toBeUndefined();
    expect(parseCompareSearch({ ids: A, threshold: "1001" }).threshold).toBeUndefined();
  });

  it("reads the regressions flag", () => {
    expect(parseCompareSearch({ ids: A, regressions: "1" }).regressions).toBe(1);
    expect(parseCompareSearch({ ids: A }).regressions).toBeUndefined();
  });
});

describe("parseExperimentsSearch and parseExperimentSearch", () => {
  it("keeps a dataset filter only when it is a UUID", () => {
    expect(parseExperimentsSearch({ dataset: A })).toEqual({ dataset: A });
    expect(parseExperimentsSearch({ dataset: "faq" })).toEqual({});
  });

  it("reads the errors flag of the experiment page", () => {
    expect(parseExperimentSearch({ errors: "1" })).toEqual({ errors: 1 });
    expect(parseExperimentSearch({})).toEqual({});
  });
});

describe("scoreSummaryText", () => {
  it("formats a numeric score", () => {
    expect(scoreSummaryText(summary({ mean: 0.82, median: 0.9, count: 40 }))).toBe(
      "mean 0.820 · median 0.900 · n 40",
    );
  });

  it("formats a boolean score as a pass rate", () => {
    expect(scoreSummaryText(summary({ data_type: "boolean", pass_rate: 0.75, count: 40 }))).toBe(
      "pass rate 75 % · n 40",
    );
    expect(scoreSummaryText(summary({ data_type: "boolean", pass_rate: 0.125, count: 8 }))).toBe(
      "pass rate 12.5 % · n 8",
    );
  });

  it("lists categorical labels by count and caps them at four", () => {
    const text = scoreSummaryText(
      summary({
        data_type: "categorical",
        labels: { a: 1, b: 9, c: 3, d: 4, e: 2 },
      }),
    );
    expect(text).toBe("b 9 · d 4 · c 3 · e 2 · +1 more");
  });

  it("formats a text score by its count", () => {
    expect(scoreSummaryText(summary({ data_type: "text", count: 40 }))).toBe("n 40");
  });
});

describe("deltaText and deltaDirection", () => {
  it("formats numeric deltas with three decimals and the minus sign", () => {
    expect(deltaText(0.12, "numeric")).toBe("+0.120");
    expect(deltaText(-0.05, "numeric")).toBe("−0.050");
    expect(deltaText(0, "numeric")).toBe("0.000");
  });

  it("formats a boolean delta in points", () => {
    expect(deltaText(0.125, "boolean")).toBe("+12.5 pts");
  });

  it("reads the direction of a delta, higher is better", () => {
    expect(deltaDirection(0.1)).toBe("better");
    expect(deltaDirection(-0.1)).toBe("worse");
    expect(deltaDirection(0)).toBe("same");
  });
});

describe("scoreValueText", () => {
  it("formats each data type", () => {
    expect(scoreValueText({ data_type: "numeric", value_num: 0.5 })).toBe("0.500");
    expect(scoreValueText({ data_type: "numeric" })).toBe("—");
    expect(scoreValueText({ data_type: "boolean", value_num: 1 })).toBe("pass");
    expect(scoreValueText({ data_type: "boolean", value_num: 0 })).toBe("fail");
    expect(scoreValueText({ data_type: "categorical", value_str: "correct" })).toBe("correct");
    expect(scoreValueText({ data_type: "text", value_str: "short" })).toBe("short");
    expect(scoreValueText({ data_type: "text", value_str: "x".repeat(41) })).toBe(
      `${"x".repeat(40)}…`,
    );
  });
});

describe("expectedRuns", () => {
  it("multiplies items by repetitions", () => {
    const experiment = {
      item_count: 3,
      repetitions: 4,
    } as Schema<"Experiment">;
    expect(expectedRuns(experiment)).toBe(12);
  });
});

describe("compareBlockReason", () => {
  const base = {
    dataset_id: "01900000-0000-7000-8000-000000000c01",
    dataset_name: "faq",
  };
  const experiment = (id: string, extra: Record<string, unknown> = {}) =>
    ({ ...base, id, ...extra }) as Schema<"Experiment">;
  const deleted = { dataset_id: undefined, dataset_name: "faq" };

  it("needs two to four experiments", () => {
    expect(compareBlockReason([])).toBe("Pick 2 to 4 experiments");
    expect(compareBlockReason([experiment("a")])).toBe("Pick 2 to 4 experiments");
    const five = ["a", "b", "c", "d", "e"].map((id) => experiment(id));
    expect(compareBlockReason(five)).toBe("Pick 2 to 4 experiments");
  });

  it("needs one dataset", () => {
    expect(compareBlockReason([experiment("a"), experiment("b")])).toBeUndefined();
    const other = experiment("c", { dataset_id: "01900000-0000-7000-8000-000000000c02" });
    expect(compareBlockReason([experiment("a"), other])).toBe(
      "Pick experiments of the same dataset",
    );
  });

  it("accepts experiments of one deleted dataset by name", () => {
    const gone = experiment("a", deleted);
    expect(compareBlockReason([gone, experiment("b", deleted)])).toBeUndefined();
    const renamed = experiment("b", { ...deleted, dataset_name: "other" });
    expect(compareBlockReason([gone, renamed])).toBe("Pick experiments of the same dataset");
    expect(compareBlockReason([gone, experiment("c")])).toBe(
      "Pick experiments of the same dataset",
    );
  });
});
