import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import ruleFixture from "../../../../fixtures/api/static/online-rule.json";
import { ruleSummary, sampleText } from "./rule-summary";

const rule = ruleFixture as Schema<"OnlineRule">;

it("summarises the target, filter and sample rate", () => {
  expect(ruleSummary(rule)).toBe("Traces · status = 'ok' · 25 %");
});

it("reads all when the rule has no filter", () => {
  expect(ruleSummary({ ...rule, filter: undefined, target: "span" })).toBe("Spans · all · 25 %");
  expect(ruleSummary({ ...rule, filter: "" })).toBe("Traces · all · 25 %");
});

it("formats the sample rate without trailing zeros", () => {
  expect(sampleText(1)).toBe("100 %");
  expect(sampleText(0.1234)).toBe("12.34 %");
});
