import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  compareBlockReason,
  compareIdList,
  datasetKey,
  deltaDirection,
  deltaText,
  expectedRuns,
  headlineText,
  headlineValue,
  isLiveExperiment,
  itemKey,
  itemShortId,
  metadataTags,
  parseCompareSearch,
  parseExperimentSearch,
  parseExperimentsSearch,
  parseItemKey,
  previousRuns,
  scoreSummaryText,
  scoreValueText,
  summaryChunks,
} from "./experiments";

const A = "01900000-0000-7000-8000-00000000ea01";
const B = "01900000-0000-7000-8000-00000000eb02";
const C = "01900000-0000-7000-8000-00000000ec03";
const D = "01900000-0000-7000-8000-00000000ed04";
const E = "01900000-0000-7000-8000-00000000ee05";

function summary(partial: Partial<Schema<"ScoreSummary">>): Schema<"ScoreSummary"> {
  return { name: "score", data_type: "numeric", count: 40, labels: {}, histogram: [], ...partial };
}

describe("parseCompareSearch", () => {
  it("keeps the first four distinct UUIDs in order", () => {
    const parsed = parseCompareSearch({ ids: `${A}, ${B},,${A},x,${C},${D},${E}` });
    expect(parsed.ids).toBe([A, B, C, D].join(","));
    expect(compareIdList(parsed.ids)).toEqual([A, B, C, D]);
  });

  it("reads no ids as an empty list", () => {
    expect(compareIdList(parseCompareSearch({}).ids)).toEqual([]);
  });

  it("reads the threshold within [0, 1000] only", () => {
    expect(parseCompareSearch({ ids: A, threshold: "0.2" }).threshold).toBe(0.2);
    expect(parseCompareSearch({ ids: A, threshold: "-1" }).threshold).toBeUndefined();
    expect(parseCompareSearch({ ids: A, threshold: "abc" }).threshold).toBeUndefined();
    expect(parseCompareSearch({ ids: A, threshold: "1001" }).threshold).toBeUndefined();
  });

  it("reads the only filter and drops other values", () => {
    expect(parseCompareSearch({ ids: A, only: "regressed" }).only).toBe("regressed");
    expect(parseCompareSearch({ ids: A, only: "improved" }).only).toBe("improved");
    expect(parseCompareSearch({ ids: A, only: "changed" }).only).toBe("changed");
    expect(parseCompareSearch({ ids: A, only: "all" }).only).toBeUndefined();
    expect(parseCompareSearch({ ids: A, only: 1 }).only).toBeUndefined();
    expect(parseCompareSearch({ ids: A }).only).toBeUndefined();
  });

  it("reads the item only as a UUID", () => {
    expect(parseCompareSearch({ ids: A, item: B }).item).toBe(B);
    expect(parseCompareSearch({ ids: A, item: "x" }).item).toBeUndefined();
    expect(parseCompareSearch({ ids: A }).item).toBeUndefined();
  });

  it("reads the router's numeric values (TanStack parses the URL as JSON)", () => {
    expect(parseCompareSearch({ ids: A, only: "improved", threshold: 0.5 }).only).toBe("improved");
    expect(parseCompareSearch({ ids: A, threshold: 0.25 }).threshold).toBe(0.25);
    expect(parseCompareSearch({ ids: A, threshold: 0 }).threshold).toBe(0);
    expect(parseCompareSearch({ ids: A, threshold: 1000 }).threshold).toBe(1000);
    expect(parseCompareSearch({ ids: A, threshold: 1001 }).threshold).toBeUndefined();
    expect(parseCompareSearch({ ids: A, threshold: -0.5 }).threshold).toBeUndefined();
  });

  it("drops a threshold that is empty or not finite", () => {
    expect(parseCompareSearch({ ids: A, threshold: "" }).threshold).toBeUndefined();
    expect(parseCompareSearch({ ids: A, threshold: "Infinity" }).threshold).toBeUndefined();
    expect(parseCompareSearch({ ids: A, threshold: Number.NaN }).threshold).toBeUndefined();
  });

  it("keeps ids only when the router gives a string", () => {
    expect(parseCompareSearch({ ids: 42 }).ids).toBe("");
    expect(parseCompareSearch({ ids: A }).ids).toBe(A);
  });
});

describe("parseExperimentsSearch and parseExperimentSearch", () => {
  it("keeps a dataset filter only when it is a UUID", () => {
    expect(parseExperimentsSearch({ dataset: A })).toEqual({ dataset: A });
    expect(parseExperimentsSearch({ dataset: "faq" })).toEqual({});
  });

  it("reads the errors flag of an open experiment", () => {
    expect(parseExperimentSearch({ errors: "1" })).toEqual({ errors: 1 });
    expect(parseExperimentSearch({ errors: 1 })).toEqual({ errors: 1 });
    expect(parseExperimentSearch({ errors: 0 })).toEqual({});
    expect(parseExperimentSearch({})).toEqual({});
  });

  it("keeps an item key only when it is a UUID, a colon and a repetition", () => {
    expect(parseExperimentSearch({ item: `${A}:0` })).toEqual({ item: `${A}:0` });
    expect(parseExperimentSearch({ item: `${A}:12` })).toEqual({ item: `${A}:12` });
    expect(parseExperimentSearch({ item: A })).toEqual({});
    expect(parseExperimentSearch({ item: `${A}:-1` })).toEqual({});
    expect(parseExperimentSearch({ item: `${A}:01` })).toEqual({});
    expect(parseExperimentSearch({ item: `faq:0` })).toEqual({});
  });
});

describe("itemKey and parseItemKey", () => {
  it("round-trips an item key", () => {
    const key = itemKey({ dataset_item_id: A, repetition: 2 });
    expect(key).toBe(`${A}:2`);
    expect(parseItemKey(key)).toEqual({ datasetItemId: A, repetition: 2 });
  });

  it("rejects malformed keys", () => {
    expect(parseItemKey("")).toBeUndefined();
    expect(parseItemKey(A)).toBeUndefined();
    expect(parseItemKey(`${A}:x`)).toBeUndefined();
    expect(parseItemKey(`${A}:1:2`)).toBeUndefined();
  });
});

describe("isLiveExperiment", () => {
  const now = Date.parse("2026-11-16T12:00:00Z");
  const experiment = (status: "running" | "completed", createdAt: string) =>
    ({ status, created_at: createdAt }) as Schema<"Experiment">;

  it("is true for a running experiment less than a day old", () => {
    expect(isLiveExperiment(experiment("running", "2026-11-16T00:00:00Z"), now)).toBe(true);
  });

  it("is false once it is a day old, when completed, or when unknown", () => {
    expect(isLiveExperiment(experiment("running", "2026-11-15T11:00:00Z"), now)).toBe(false);
    expect(isLiveExperiment(experiment("completed", "2026-11-16T11:00:00Z"), now)).toBe(false);
    expect(isLiveExperiment(undefined, now)).toBe(false);
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
    expect(deltaDirection(0.1, "numeric")).toBe("better");
    expect(deltaDirection(-0.1, "numeric")).toBe("worse");
    expect(deltaDirection(0, "numeric")).toBe("same");
  });

  it("reads a delta that prints as zero as same", () => {
    expect(deltaText(-0.0004, "numeric")).toBe("0.000");
    expect(deltaDirection(-0.0004, "numeric")).toBe("same");
    expect(deltaDirection(0.0004, "numeric")).toBe("same");
  });

  it("reads a boolean delta that prints as 0.0 pts as same", () => {
    expect(deltaText(-0.0001, "boolean")).toBe("0.0 pts");
    expect(deltaDirection(-0.0001, "boolean")).toBe("same");
    expect(deltaDirection(-0.01, "boolean")).toBe("worse");
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

describe("parseExperimentsSearch status and experiment", () => {
  it("keeps a known status and a UUID experiment, and drops the rest", () => {
    expect(parseExperimentsSearch({ status: "failed", experiment: A })).toEqual({
      status: "failed",
      experiment: A,
    });
    expect(parseExperimentsSearch({ status: "done", experiment: "nope" })).toEqual({});
  });

  it("keeps errors and item only with an open experiment", () => {
    const item = `${A}:2`;
    expect(parseExperimentsSearch({ experiment: A, errors: "1", item })).toEqual({
      experiment: A,
      errors: 1,
      item,
    });
    expect(parseExperimentsSearch({ errors: "1", item })).toEqual({});
    expect(parseExperimentsSearch({ experiment: "nope", errors: "1", item })).toEqual({});
  });
});

describe("datasetKey", () => {
  it("is the dataset id, or the name of a deleted dataset", () => {
    expect(datasetKey({ dataset_id: A, dataset_name: "faq" } as Schema<"Experiment">)).toBe(A);
    expect(datasetKey({ dataset_name: "faq" } as Schema<"Experiment">)).toBe("deleted:faq");
  });
});

describe("previousRuns", () => {
  const run = (id: string, extra: Record<string, unknown> = {}) =>
    ({ id, dataset_id: A, dataset_name: "faq", ...extra }) as Schema<"Experiment">;

  it("maps each run to the next older run of its dataset, newest first", () => {
    const rows = [run(C), run(B, { dataset_id: D }), run(B.replace("eb02", "eb03")), run(A)];
    const previous = previousRuns(rows);
    expect(previous.get(C)).toBe(B.replace("eb02", "eb03"));
    expect(previous.get(B.replace("eb02", "eb03"))).toBe(A);
    expect(previous.has(A)).toBe(false);
    expect(previous.has(B)).toBe(false);
  });

  it("treats a deleted dataset's runs as one dataset by name", () => {
    const rows = [run(C, { dataset_id: undefined }), run(B, { dataset_id: undefined })];
    expect(previousRuns(rows).get(C)).toBe(B);
  });
});

describe("headlineValue and headlineText", () => {
  it("headlines numeric by mean and boolean by pass rate", () => {
    const numeric = summary({ data_type: "numeric", mean: 0.5 });
    expect(headlineValue(numeric)).toBe(0.5);
    expect(headlineText(numeric)).toBe("0.500");
    const boolean = summary({ data_type: "boolean", pass_rate: 2 / 3 });
    expect(headlineValue(boolean)).toBeCloseTo(2 / 3);
    expect(headlineText(boolean)).toBe("66.7 %");
    expect(headlineText(summary({ data_type: "boolean", pass_rate: 1 }))).toBe("100 %");
  });

  it("has no headline value for categorical or text scores", () => {
    expect(headlineValue(summary({ data_type: "categorical" }))).toBeUndefined();
    expect(headlineText(summary({ data_type: "categorical" }))).toBe("—");
    expect(headlineText(summary({ data_type: "text", count: 0 }))).toBe("—");
    expect(headlineText(summary({ data_type: "numeric" }))).toBe("—");
  });

  it("headlines a categorical score by its top label and a text score by its count", () => {
    const categorical = summary({
      data_type: "categorical",
      labels: { formal: 1, friendly: 2, curt: 2 },
    });
    expect(headlineText(categorical)).toBe("curt 40 %");
    expect(headlineText(summary({ data_type: "text", count: 12 }))).toBe("n 12");
  });
});

describe("itemShortId", () => {
  it("keeps the last 8 characters", () => {
    expect(itemShortId("01900000-0000-7000-8000-000000000d01")).toBe("00000d01");
  });
});

describe("metadataTags", () => {
  it("keeps strings, numbers and booleans in order, and cuts long strings", () => {
    expect(
      metadataTags({
        model: "gpt",
        temperature: 0.2,
        cached: false,
        nested: { a: 1 },
        empty: null,
        prompt: "x".repeat(41),
      }),
    ).toEqual([
      ["model", "gpt"],
      ["temperature", "0.2"],
      ["cached", "false"],
      ["prompt", `${"x".repeat(40)}…`],
    ]);
  });
});

describe("summaryChunks", () => {
  it("splits the rows into runs of at most 25, in order", () => {
    const rows = Array.from({ length: 51 }, (_, index) => index);
    const chunks = summaryChunks(rows);
    expect(chunks.map((chunk) => chunk.length)).toEqual([25, 25, 1]);
    expect(chunks.flat()).toEqual(rows);
    expect(summaryChunks([])).toEqual([]);
  });
});
