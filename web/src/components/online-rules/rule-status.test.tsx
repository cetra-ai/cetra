import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { TooltipProvider } from "@/components/ui/tooltip";
import ruleFixture from "../../../../fixtures/api/static/online-rule.json";
import { RuleStatusPill, ruleBucket } from "./rule-status";

const rule = ruleFixture as Schema<"OnlineRule">;

it("buckets a rule as active, paused or disabled", () => {
  expect(ruleBucket(rule)).toBe("active");
  expect(ruleBucket({ enabled: true, paused_reason: "budget" })).toBe("paused");
  expect(ruleBucket({ enabled: false, paused_reason: "errors" })).toBe("disabled");
});

it("renders each status label", () => {
  const labels: [Partial<Schema<"OnlineRule">>, string][] = [
    [{}, "Active"],
    [{ paused_reason: "budget" }, "Paused: budget"],
    [{ paused_reason: "errors" }, "Paused: errors"],
    [{ paused_reason: "connection" }, "Paused: connection"],
    [{ enabled: false }, "Disabled"],
  ];
  for (const [overrides, label] of labels) {
    const { unmount } = render(
      <TooltipProvider>
        <RuleStatusPill rule={{ ...rule, paused_detail: undefined, ...overrides }} />
      </TooltipProvider>,
    );
    expect(screen.getByText(label)).toBeInTheDocument();
    unmount();
  }
});
