import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { selectionTotals, type Totals, TraceTotalsLine } from "@/components/trace/trace-totals";
import tracesFixture from "../../../../fixtures/api/responses/traces.json";

const rows = tracesFixture.data as Schema<"TraceSummary">[];

function renderLine(props: {
  selected?: Totals;
  list?: Totals;
  listStatus?: "pending" | "error" | "success";
}): HTMLElement {
  render(
    <TraceTotalsLine
      selected={props.selected}
      list={props.list}
      listStatus={props.listStatus ?? "success"}
    />,
  );
  return screen.getByRole("status", { name: "Totals" });
}

it("shows the list totals", () => {
  const totals = renderLine({ list: { traceCount: 203, errorTraceCount: 3, costTotal: 4.12 } });
  expect(totals.textContent).toBe("203 traces · 3 errors · $4.12");
  expect(screen.getByText("3 errors")).toHaveClass("text-destructive");
});

it("uses the singular and groups thousands", () => {
  const single = renderLine({ list: { traceCount: 1, errorTraceCount: 1, costTotal: 0 } });
  expect(single.textContent).toBe("1 trace · 1 error · $0");
});

it("groups thousands and leaves a zero error count unmarked", () => {
  const totals = renderLine({ list: { traceCount: 1204, errorTraceCount: 0, costTotal: 12.5 } });
  expect(totals.textContent).toBe("1,204 traces · 0 errors · $12.50");
  expect(screen.getByText("0 errors")).not.toHaveClass("text-destructive");
});

it("shows counting while the list totals load", () => {
  expect(renderLine({ listStatus: "pending" }).textContent).toBe("Counting…");
});

it("shows unavailable when the list totals fail", () => {
  expect(renderLine({ listStatus: "error" }).textContent).toBe("Totals unavailable");
});

it("shows the selection against the list", () => {
  const totals = renderLine({
    selected: { traceCount: 2, errorTraceCount: 1, costTotal: 0.0027 },
    list: { traceCount: 203, errorTraceCount: 3, costTotal: 4.12 },
    listStatus: "success",
  });
  expect(totals.textContent).toBe("2 of 203 selected · 1 error · $0.0027");
});

it("shows the selection alone while the list totals are pending", () => {
  const totals = renderLine({
    selected: { traceCount: 2, errorTraceCount: 1, costTotal: 0.0027 },
    listStatus: "pending",
  });
  expect(totals.textContent).toBe("2 selected · 1 error · $0.0027");
});

it("selectionTotals sums the checked rows", () => {
  const ids = new Set([
    "a1000000000000000000000000000006",
    "a1000000000000000000000000000003",
    "a10000000000000000000000000000ff",
  ]);
  expect(selectionTotals(rows, ids)).toEqual({
    traceCount: 2,
    errorTraceCount: 1,
    costTotal: 0.0027,
  });
  expect(selectionTotals(rows, new Set())).toEqual({
    traceCount: 0,
    errorTraceCount: 0,
    costTotal: 0,
  });
});
