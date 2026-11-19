import { describe, expect, it } from "vitest";
import { axisTicks, formatValue, measureKind, measureLabel } from "./format";

describe("measureKind", () => {
  it("counts counts and distinct counts, and rates rates", () => {
    expect(measureKind({ agg: "count" })).toBe("count");
    expect(measureKind({ agg: "count_distinct", field: "user_id" })).toBe("count");
    expect(measureKind({ agg: "rate", where: "status = 'error'" })).toBe("rate");
  });

  it("formats a field by its kind: durations, costs, scores, tokens as counts", () => {
    expect(measureKind({ agg: "p95", field: "duration_ns" })).toBe("duration");
    expect(measureKind({ agg: "sum", field: "cost_total" })).toBe("cost");
    expect(measureKind({ agg: "avg", field: "value_num" })).toBe("score");
    expect(measureKind({ agg: "sum", field: "total_tokens" })).toBe("count");
  });
});

describe("measureLabel", () => {
  it("uses the label, else the aggregate and field", () => {
    expect(measureLabel({ agg: "sum", field: "cost_total", label: "Cost" })).toBe("Cost");
    expect(measureLabel({ agg: "p95", field: "duration_ns" })).toBe("p95 of duration_ns");
    expect(measureLabel({ agg: "count" })).toBe("count");
    expect(measureLabel({ agg: "rate", where: "status = 'error'" })).toBe("rate");
  });
});

describe("formatValue", () => {
  it("formats each kind, and a gap as a dash", () => {
    expect(formatValue("duration", 4_200_000_000)).toBe("4.20s");
    expect(formatValue("cost", 0.00955)).toBe("$0.00955");
    expect(formatValue("rate", 0.2)).toBe("20%");
    expect(formatValue("count", 1234)).toBe("1,234");
    expect(formatValue("score", 0.456)).toBe("0.46");
    expect(formatValue("count", null)).toBe("—");
    expect(formatValue("count", undefined)).toBe("—");
  });
});

describe("axisTicks", () => {
  it("draws whole-number ticks for counts and percentages for rates", () => {
    expect(axisTicks("count", 3).ticks.every((tick) => Number.isInteger(tick))).toBe(true);
    const rate = axisTicks("rate", 0.2);
    expect(rate.ticks.at(-1)).toBeGreaterThanOrEqual(0.2);
    expect(rate.format(0.05)).toBe("5%");
  });

  it("gives every tick its label", () => {
    const scale = axisTicks("duration", 4_200_000_000);
    expect(scale.ticks.at(-1)).toBeGreaterThanOrEqual(4_200_000_000);
    expect(scale.format(1_000_000_000)).toMatch(/s$/);
  });
});
