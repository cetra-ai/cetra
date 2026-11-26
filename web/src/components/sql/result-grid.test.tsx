import { fireEvent, render, screen, within } from "@testing-library/react";
import { Braces, Calendar, Clock, Hash, ToggleLeft, Type } from "lucide-react";
import { afterEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { SqlResultGrid, sqlColumns } from "@/components/sql/result-grid";

// The generated schema types each row as `Record<string, never>`; real rows are JSON values.
// Same cast as lib/sql/chart.test.ts.
const ROWS: Record<string, unknown>[] = [
  { name: "b", n: 2, tags: ["x"] },
  { name: "a", n: 10, tags: [] },
  { name: "c", n: null, tags: null },
];
const RESULT: Schema<"SqlQueryResult"> = {
  columns: [
    { name: "name", type: "string" },
    { name: "n", type: "integer" },
    { name: "tags", type: "string_list" },
  ],
  rows: ROWS as Schema<"SqlQueryResult">["rows"],
  truncated: false,
};

/** The body row at `index` (0-based), skipping the header row. */
function bodyRow(index: number): HTMLElement {
  const row = screen.getAllByRole("row")[index + 1];
  if (!row) {
    throw new Error(`no body row ${index}`);
  }
  return row;
}

function firstCellOf(row: HTMLElement): string | null | undefined {
  return within(row).getAllByRole("gridcell")[0]?.textContent;
}

afterEach(() => {
  vi.restoreAllMocks();
});

it("builds one column per result column, aligned and iconed by type", () => {
  const columns = sqlColumns({
    columns: [
      { name: "a b", type: "string" },
      { name: "n", type: "integer" },
      { name: "when", type: "timestamp" },
      { name: "day", type: "date" },
      { name: "ok", type: "boolean" },
      { name: "xs", type: "list" },
      { name: "raw", type: "binary" },
    ],
    rows: [],
    truncated: false,
  });
  expect(columns.map((column) => column.id)).toEqual(["c0", "c1", "c2", "c3", "c4", "c5", "c6"]);
  expect(columns.map((column) => column.meta?.label)).toEqual([
    "a b",
    "n",
    "when",
    "day",
    "ok",
    "xs",
    "raw",
  ]);
  expect(columns.map((column) => column.meta?.align)).toEqual([
    "left",
    "right",
    "left",
    "left",
    "left",
    "left",
    "left",
  ]);
  expect(columns.map((column) => column.meta?.icon)).toEqual([
    Type,
    Hash,
    Clock,
    Calendar,
    ToggleLeft,
    Braces,
    undefined,
  ]);
});

it("makes every column sortable and lets only the last one grow", () => {
  const columns = sqlColumns(RESULT);
  expect(columns.every((column) => column.enableSorting === true)).toBe(true);
  expect(columns.map((column) => column.meta?.grow)).toEqual([false, false, true]);
});

it("shows the column type as a tooltip on the header", () => {
  render(<SqlResultGrid result={RESULT} />);
  expect(screen.getByTitle("integer")).toHaveTextContent("n");
});

it("cycles a column's sort descending, ascending, then none on header clicks", () => {
  render(<SqlResultGrid result={RESULT} />);
  const header = screen.getByRole("button", { name: "n" });
  const sortOf = () => screen.getByRole("columnheader", { name: "n" }).getAttribute("aria-sort");

  fireEvent.click(header);
  expect(sortOf()).toBe("descending");
  expect(firstCellOf(bodyRow(0))).toBe("a");

  fireEvent.click(header);
  expect(sortOf()).toBe("ascending");
  expect(firstCellOf(bodyRow(0))).toBe("b");

  fireEvent.click(header);
  expect(sortOf()).toBe("none");
  expect(firstCellOf(bodyRow(0))).toBe("b");
});

it("opens a sheet with the pretty-printed value when a row is clicked", () => {
  render(<SqlResultGrid result={RESULT} />);
  fireEvent.click(bodyRow(0));

  const dialog = screen.getByRole("dialog");
  expect(within(dialog).getByText("Row 1")).toBeInTheDocument();
  const pres = Array.from(dialog.querySelectorAll("pre")).map((pre) => pre.textContent);
  expect(pres).toContain('[\n  "x"\n]');
});

it("numbers the open row by its position in the result, not the sorted grid", () => {
  render(<SqlResultGrid result={RESULT} />);
  fireEvent.click(screen.getByRole("button", { name: "n" }));
  fireEvent.click(bodyRow(0));
  expect(within(screen.getByRole("dialog")).getByText("Row 2")).toBeInTheDocument();
});

it("copies the whole row as JSON", () => {
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", {
    value: { writeText },
    configurable: true,
  });
  render(<SqlResultGrid result={RESULT} />);
  fireEvent.click(bodyRow(0));
  fireEvent.click(screen.getByRole("button", { name: "Copy row as JSON" }));
  expect(writeText).toHaveBeenCalledWith(JSON.stringify(RESULT.rows[0], null, 2));
});

it("renders duplicate column names without a React key warning", () => {
  const errors = vi.spyOn(console, "error").mockImplementation(() => {});
  // Same cast as ROWS above: real rows are JSON values, which the generated type does not show.
  const duplicateRows: Record<string, unknown>[] = [{ a: 2 }];
  const result: Schema<"SqlQueryResult"> = {
    columns: [
      { name: "a", type: "integer" },
      { name: "a", type: "integer" },
    ],
    rows: duplicateRows as Schema<"SqlQueryResult">["rows"],
    truncated: false,
  };
  render(<SqlResultGrid result={result} />);
  fireEvent.click(bodyRow(0));

  const dialog = screen.getByRole("dialog");
  // Both entries show the one value the row object holds for the name.
  expect(within(dialog).getAllByText("2")).toHaveLength(2);
  // React reports a duplicate key as "Encountered two children with the same key".
  const keyWarnings = errors.mock.calls.filter((call) =>
    /same key|unique "key"/.test(String(call[0])),
  );
  expect(keyWarnings).toEqual([]);
});

it("shows the next row when another row is opened after closing", () => {
  render(<SqlResultGrid result={RESULT} />);
  fireEvent.click(bodyRow(0));
  fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
  expect(screen.queryByRole("dialog")).toBeNull();

  fireEvent.click(bodyRow(1));
  expect(within(screen.getByRole("dialog")).getByText("Row 2")).toBeInTheDocument();
});

it("clears the sort and the open row when a new result arrives", () => {
  const { rerender } = render(<SqlResultGrid result={RESULT} />);
  fireEvent.click(screen.getByRole("button", { name: "n" }));
  fireEvent.click(bodyRow(0));
  expect(screen.getByRole("dialog")).toBeInTheDocument();

  rerender(<SqlResultGrid result={{ ...RESULT, rows: [...RESULT.rows] }} />);
  expect(screen.queryByRole("dialog")).toBeNull();
  expect(screen.getByRole("columnheader", { name: "n" }).getAttribute("aria-sort")).toBe("none");
});
