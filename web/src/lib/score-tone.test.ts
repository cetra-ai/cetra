import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { averageGoodness, goodness, toneOf } from "@/lib/score-tone";

type Definition = Schema<"ScoreDefinition">;

const BASE = {
  id: "01900000-0000-7000-8000-000000000001",
  name: "score",
  categories: [],
  created_at: "2026-01-01T09:00:00.000000000Z",
  updated_at: "2026-01-01T09:00:00.000000000Z",
};

function numeric(direction: Definition["direction"], min = 0, max = 1): Definition {
  return { ...BASE, data_type: "numeric", direction, min_value: min, max_value: max };
}

function boolean(direction: Definition["direction"]): Definition {
  return { ...BASE, data_type: "boolean", direction };
}

function categorical(direction: Definition["direction"]): Definition {
  return {
    ...BASE,
    data_type: "categorical",
    direction,
    categories: [
      { label: "bad", value: 0 },
      { label: "ok", value: 1 },
      { label: "great", value: 3 },
    ],
  };
}

describe("goodness", () => {
  it("places a numeric value in its range, and flips it for lower", () => {
    expect(goodness({ data_type: "numeric", value: 0.9 }, numeric("higher"))).toBeCloseTo(0.9);
    expect(goodness({ data_type: "numeric", value: 0.9 }, numeric("lower"))).toBeCloseTo(0.1);
  });

  it("clamps values outside the range", () => {
    expect(goodness({ data_type: "numeric", value: 4 }, numeric("higher"))).toBe(1);
    expect(goodness({ data_type: "numeric", value: -2 }, numeric("higher"))).toBe(0);
  });

  it("is undefined without a direction, a matching type or a range", () => {
    expect(goodness({ data_type: "numeric", value: 0.9 }, numeric("none"))).toBeUndefined();
    expect(goodness({ data_type: "boolean", value: true }, numeric("higher"))).toBeUndefined();
    const open = { ...numeric("higher"), max_value: undefined };
    expect(goodness({ data_type: "numeric", value: 0.9 }, open)).toBeUndefined();
    expect(goodness({ data_type: "numeric", value: 0.9 }, numeric("higher", 1, 1))).toBeUndefined();
  });

  it("scores booleans as 1 for true and 0 for false", () => {
    expect(goodness({ data_type: "boolean", value: true }, boolean("higher"))).toBe(1);
    expect(goodness({ data_type: "boolean", value: false }, boolean("higher"))).toBe(0);
    expect(goodness({ data_type: "boolean", value: false }, boolean("lower"))).toBe(1);
  });

  it("places a categorical label through the category values", () => {
    const ok = { data_type: "categorical", value: { label: "ok" } } as const;
    expect(goodness(ok, categorical("higher"))).toBeCloseTo(1 / 3);
    expect(goodness(ok, categorical("lower"))).toBeCloseTo(2 / 3);
    const great = { data_type: "categorical", value: { label: "great" } } as const;
    expect(goodness(great, categorical("higher"))).toBe(1);
  });

  it("returns undefined for text scores", () => {
    const text = { ...BASE, data_type: "text", direction: "higher" } as Definition;
    expect(goodness({ data_type: "text", value: "x" }, text)).toBeUndefined();
  });
});

describe("averageGoodness", () => {
  it("uses a boolean average as the goodness directly", () => {
    expect(averageGoodness(0.75, "boolean", boolean("higher"))).toBe(0.75);
    expect(averageGoodness(0.75, "boolean", boolean("lower"))).toBeCloseTo(0.25);
  });

  it("places a numeric average in its range", () => {
    expect(averageGoodness(0.2, "numeric", numeric("higher"))).toBeCloseTo(0.2);
    expect(averageGoodness(0.2, "numeric", numeric("lower"))).toBeCloseTo(0.8);
    expect(averageGoodness(0.2, "numeric", numeric("none"))).toBeUndefined();
    expect(averageGoodness(0.2, "boolean", numeric("higher"))).toBeUndefined();
  });
});

describe("toneOf", () => {
  it("switches at exactly one third and two thirds", () => {
    expect(toneOf(1 / 3)).toBe("bad");
    expect(toneOf(0.34)).toBe("mid");
    expect(toneOf(2 / 3)).toBe("good");
    expect(toneOf(0.66)).toBe("mid");
    expect(toneOf(0)).toBe("bad");
    expect(toneOf(1)).toBe("good");
  });
});
