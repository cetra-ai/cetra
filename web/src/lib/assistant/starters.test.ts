import { expect, it } from "vitest";
import type { PageKind } from "@/lib/assistant/context";
import { fieldLabel, PAGE_LABEL, placeholderOf, startersOf } from "@/lib/assistant/starters";

it("every page kind has a label, a placeholder and three distinct starters", () => {
  for (const page of Object.keys(PAGE_LABEL) as PageKind[]) {
    expect(placeholderOf(page)).toMatch(/^Ask about .+…$/);
    const starters = startersOf(page);
    expect(starters).toHaveLength(3);
    expect(new Set(starters).size).toBe(3);
  }
});

it("a known field has a readable label and an unknown one keeps its key", () => {
  expect(fieldLabel("trace_id")).toBe("Trace");
  expect(fieldLabel("include_eval")).toBe("Evaluation traces");
  expect(fieldLabel("something_new")).toBe("something_new");
});
