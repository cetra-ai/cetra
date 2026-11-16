import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { TreeGrid } from "@/components/trace/tree-grid";

const TRACE = {
  trace_id: "a1",
  name: "root",
  start_time: "2026-01-01T11:00:00.000Z",
  end_time: "2026-01-01T11:00:02.000Z",
} as Schema<"TraceSummary">;

const SPANS = [
  {
    depth: 0,
    detached: false,
    span: {
      span_id: "s1",
      parent_span_id: undefined,
      name: "root",
      span_kind: "agent",
      status: "ok",
      start_time: "2026-01-01T11:00:00.000Z",
      end_time: "2026-01-01T11:00:02.000Z",
      duration_ns: 2_000_000_000,
    },
  },
] as unknown as Schema<"TreeSpan">[];

function renderGrid() {
  render(
    <TreeGrid
      spans={SPANS}
      trace={TRACE}
      selectedSpanId="s1"
      onSelectSpan={() => {}}
      mode="tree"
      onMode={() => {}}
    />,
  );
  return screen.getByRole("separator", { name: "Resize Name column" });
}

afterEach(() => {
  localStorage.clear();
});

it("starts at the content-fitted width, never below 220px", () => {
  expect(renderGrid()).toHaveAttribute("aria-valuenow", "220");
});

it("arrow keys resize the Name column and store the width", () => {
  const handle = renderGrid();
  fireEvent.keyDown(handle, { key: "ArrowRight" });
  expect(handle).toHaveAttribute("aria-valuenow", "236");
  expect(localStorage.getItem("cetra.tree.name_width")).toBe("236");
  fireEvent.keyDown(handle, { key: "ArrowLeft" });
  fireEvent.keyDown(handle, { key: "ArrowLeft" });
  expect(handle).toHaveAttribute("aria-valuenow", "204");
});

it("never goes below 140px", () => {
  const handle = renderGrid();
  for (let i = 0; i < 20; i += 1) {
    fireEvent.keyDown(handle, { key: "ArrowLeft" });
  }
  expect(handle).toHaveAttribute("aria-valuenow", "140");
});

it("restores a stored width, ignores an invalid one, and double-click resets", () => {
  localStorage.setItem("cetra.tree.name_width", "300");
  const handle = renderGrid();
  expect(handle).toHaveAttribute("aria-valuenow", "300");
  fireEvent.doubleClick(handle);
  expect(handle).toHaveAttribute("aria-valuenow", "220");
  expect(localStorage.getItem("cetra.tree.name_width")).toBeNull();
});

it("falls back to the content-fitted width for an invalid stored width", () => {
  localStorage.setItem("cetra.tree.name_width", "10");
  expect(renderGrid()).toHaveAttribute("aria-valuenow", "220");
});
