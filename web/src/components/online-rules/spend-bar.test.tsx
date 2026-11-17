import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import ruleFixture from "../../../../fixtures/api/static/online-rule.json";
import { SpendBar, spendRatio } from "./spend-bar";

const NOW = "2026-01-01T00:00:00.000000000Z";

type Rule = Schema<"OnlineRule">;

const rule = ruleFixture as Rule;

function withSpent(spent_usd: number): Rule {
  return { ...rule, spend: { period_start: NOW, spent_usd, reserved_usd: 0 } };
}

function fillClass(spent: number) {
  const { unmount } = render(<SpendBar rule={withSpent(spent)} />);
  const fill = screen.getByRole("progressbar", { name: "Spend" }).firstElementChild as HTMLElement;
  const className = fill.className;
  unmount();
  return className;
}

it("colours the fill by how much of the budget is spent", () => {
  expect(fillClass(3.99)).toContain("bg-primary");
  expect(fillClass(4)).toContain("bg-status-warn");
  expect(fillClass(4.99)).toContain("bg-status-warn");
  expect(fillClass(5)).toContain("bg-status-error");
  expect(fillClass(7)).toContain("bg-status-error");
});

it("caps the fill width at 100 %", () => {
  render(<SpendBar rule={withSpent(9)} />);
  const bar = screen.getByRole("progressbar", { name: "Spend" });
  expect(bar).toHaveAttribute("aria-valuenow", "100");
  expect(bar.firstElementChild).toHaveStyle({ width: "100%" });
});

it("shows the spend against the budget and period", () => {
  render(<SpendBar rule={rule} />);
  expect(screen.getByText("$0.42 / $5.00 today")).toBeInTheDocument();
});

it("shows a dash without a budget", () => {
  const { budget_usd: _usd, budget_period: _period, ...withoutBudget } = rule;
  render(<SpendBar rule={withoutBudget} />);
  expect(screen.getByText("—")).toBeInTheDocument();
  expect(screen.queryByRole("progressbar")).toBeNull();
  expect(spendRatio(withoutBudget)).toBeUndefined();
});
