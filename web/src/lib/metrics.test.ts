import { expect, it } from "vitest";
import { kpiDelta } from "@/lib/metrics";

it("reports kpi direction and leaves the percentage undefined when the previous value is 0", () => {
  expect(kpiDelta({ value: 10, previous: 5 })).toEqual({
    value: 10,
    previous: 5,
    changePct: 100,
    direction: "up",
  });
  expect(kpiDelta({ value: 4, previous: 8 })).toEqual({
    value: 4,
    previous: 8,
    changePct: -50,
    direction: "down",
  });
  expect(kpiDelta({ value: 3, previous: 3 })).toMatchObject({ direction: "flat", changePct: 0 });
  expect(kpiDelta({ value: 5, previous: 0 })).toEqual({
    value: 5,
    previous: 0,
    isNew: true,
    direction: "up",
  });
  expect(kpiDelta({ value: 0, previous: 0 })).toEqual({
    value: 0,
    previous: 0,
    direction: "flat",
  });
  expect(kpiDelta({ value: 5 })).toEqual({ value: 5, direction: "flat" });
  expect(kpiDelta({ previous: 5 })).toEqual({ previous: 5, direction: "flat" });
});
