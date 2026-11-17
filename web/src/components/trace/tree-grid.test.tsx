import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
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

function gridElement(timeline: boolean) {
  return (
    <QueryClientProvider client={new QueryClient()}>
      <ProjectProvider
        value={{ org: "acme", project: "support-bot", ref: "p1", data: {} as Schema<"Project"> }}
      >
        <TreeGrid
          spans={SPANS}
          trace={TRACE}
          selectedSpanId="s1"
          onSelectSpan={() => {}}
          mode={timeline ? "timeline" : "tree"}
          onMode={() => {}}
          timeline={timeline}
        />
      </ProjectProvider>
    </QueryClientProvider>
  );
}

function renderGrid() {
  render(gridElement(true));
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

it("tree mode has no time axis, and timeline mode adds it back", () => {
  const { rerender } = render(gridElement(false));
  expect(screen.queryByRole("columnheader", { name: "Timeline" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Timeline root" })).not.toBeInTheDocument();
  expect(screen.queryByRole("separator", { name: "Resize Name column" })).not.toBeInTheDocument();
  expect(screen.getByRole("grid", { name: "Trace" })).toHaveAttribute("aria-colcount", "4");

  rerender(gridElement(true));
  expect(screen.getByRole("columnheader", { name: "Timeline" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Timeline root" })).toBeInTheDocument();
  expect(screen.getByRole("grid", { name: "Trace" })).toHaveAttribute("aria-colcount", "5");
});

it("falls back to the content-fitted width for an invalid stored width", () => {
  localStorage.setItem("cetra.tree.name_width", "10");
  expect(renderGrid()).toHaveAttribute("aria-valuenow", "220");
});
