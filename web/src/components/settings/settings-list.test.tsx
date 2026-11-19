import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import {
  filterRows,
  SettingsListLayout,
  SettingsListToolbar,
  SettingsPanel,
  SettingsTableFrame,
  sortRows,
} from "./settings-list";

type Row = { name: string; uses?: number; email?: string };

const ROWS: Row[] = [
  { name: "Bravo", uses: 2, email: "b@example.com" },
  { name: "alpha", uses: undefined, email: "a@example.com" },
  { name: "Charlie", uses: 10 },
  { name: "delta", uses: 2 },
];

it("filters on a trimmed, case-insensitive substring of any text", () => {
  expect(filterRows(ROWS, "  ALP ", (row) => [row.name])).toEqual([ROWS[1]]);
  expect(filterRows(ROWS, "example", (row) => [row.name, row.email])).toEqual([ROWS[0], ROWS[1]]);
});

it("returns every row for an empty or missing query", () => {
  expect(filterRows(ROWS, "  ", (row) => [row.name])).toEqual(ROWS);
  expect(filterRows(ROWS, undefined, (row) => [row.name])).toEqual(ROWS);
});

it("skips undefined text values", () => {
  expect(filterRows(ROWS, "zz", (row) => [row.email])).toEqual([]);
});

const KEYS = {
  name: (row: Row) => row.name,
  uses: (row: Row) => row.uses,
};

it("returns the input order for no sorting or an unknown column", () => {
  expect(sortRows(ROWS, undefined, KEYS)).toEqual(ROWS);
  expect(sortRows(ROWS, { id: "nope", desc: false }, KEYS)).toEqual(ROWS);
  expect(sortRows(ROWS, undefined, KEYS)).not.toBe(ROWS);
});

it("sorts text case-insensitively with numeric collation", () => {
  const named = [{ name: "item 10" }, { name: "Item 2" }, { name: "apple" }];
  const sorted = sortRows(
    named,
    { id: "name", desc: false },
    {
      name: (row) => row.name,
    },
  );
  expect(sorted.map((row) => row.name)).toEqual(["apple", "Item 2", "item 10"]);
});

it("sorts numbers and reverses for desc", () => {
  expect(sortRows(ROWS, { id: "uses", desc: false }, KEYS).map((row) => row.name)).toEqual([
    "Bravo",
    "delta",
    "Charlie",
    "alpha",
  ]);
  expect(sortRows(ROWS, { id: "uses", desc: true }, KEYS).map((row) => row.name)).toEqual([
    "Charlie",
    "Bravo",
    "delta",
    "alpha",
  ]);
});

it("keeps undefined values last in both directions", () => {
  const asc = sortRows(ROWS, { id: "uses", desc: false }, KEYS);
  const desc = sortRows(ROWS, { id: "uses", desc: true }, KEYS);
  expect(asc.at(-1)?.name).toBe("alpha");
  expect(desc.at(-1)?.name).toBe("alpha");
});

it("is stable for equal values", () => {
  expect(
    sortRows(ROWS, { id: "uses", desc: false }, KEYS)
      .slice(0, 2)
      .map((row) => row.name),
  ).toEqual(["Bravo", "delta"]);
});

it("does not mutate the input", () => {
  const copy = [...ROWS];
  sortRows(ROWS, { id: "name", desc: true }, KEYS);
  expect(ROWS).toEqual(copy);
});

it("labels the search box and reports the count", async () => {
  const user = userEvent.setup();
  const onQueryChange = vi.fn();
  render(
    <SettingsListToolbar label="API keys" query="" onQueryChange={onQueryChange} count="3 keys">
      <button type="button">Filter</button>
    </SettingsListToolbar>,
  );

  await user.type(screen.getByRole("searchbox", { name: "Search API keys" }), "a");
  expect(onQueryChange).toHaveBeenCalledWith("a");
  expect(screen.getByText("3 keys")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Filter" })).toBeInTheDocument();
});

it("sizes the table frame to the rows, between 3 and 12", () => {
  const { container, rerender } = render(
    <SettingsTableFrame rowCount={0}>
      <div />
    </SettingsTableFrame>,
  );
  // Compact rows are 28px outside a ThemeProvider: (3 + 1) * 28 + 2.
  expect(container.firstElementChild).toHaveStyle({ height: "126px" });

  rerender(
    <SettingsTableFrame rowCount={100}>
      <div />
    </SettingsTableFrame>,
  );
  expect(container.firstElementChild).toHaveStyle({ height: "378px" });
});

it("closes from its close button", async () => {
  const user = userEvent.setup();
  const onClose = vi.fn();
  render(
    <SettingsPanel title="Key one" subtitle="sk-cetra-…" onClose={onClose}>
      <p>Body</p>
    </SettingsPanel>,
  );

  expect(screen.getByRole("heading", { name: "Key one" })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Close panel" }));
  expect(onClose).toHaveBeenCalledTimes(1);
});

it("closes the panel on Esc while it is open", () => {
  const onClose = vi.fn();
  render(
    <SettingsListLayout open onClose={onClose} panel={<p>Panel</p>}>
      <input aria-label="Filter" />
    </SettingsListLayout>,
  );

  fireEvent.keyDown(document.body, { key: "Escape" });
  expect(onClose).toHaveBeenCalledTimes(1);
});

it("keeps Esc inside a text field", () => {
  const onClose = vi.fn();
  render(
    <SettingsListLayout open onClose={onClose} panel={<p>Panel</p>}>
      <input aria-label="Filter" />
    </SettingsListLayout>,
  );

  fireEvent.keyDown(screen.getByLabelText("Filter"), { key: "Escape" });
  expect(onClose).not.toHaveBeenCalled();
});

it("ignores Esc while the panel is closed", () => {
  const onClose = vi.fn();
  render(
    <SettingsListLayout open={false} onClose={onClose} panel={<p>Panel</p>}>
      <p>List</p>
    </SettingsListLayout>,
  );

  fireEvent.keyDown(document.body, { key: "Escape" });
  expect(onClose).not.toHaveBeenCalled();
});
