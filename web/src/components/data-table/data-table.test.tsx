import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { Clock } from "lucide-react";
import { afterEach, expect, it, vi } from "vitest";
import { DataTable, type DataTableColumn, ROW_HEIGHT } from "@/components/data-table/data-table";

interface Item {
  id: string;
  name: string;
  failed: boolean;
}

const columns: DataTableColumn<Item>[] = [
  {
    id: "name",
    size: 200,
    header: "Name",
    accessorFn: (row) => row.name,
    enableSorting: true,
    meta: { icon: Clock },
    cell: ({ row }) => row.original.name,
  },
];

const ITEMS: Item[] = [
  { id: "a", name: "fine", failed: false },
  { id: "b", name: "broken", failed: true },
];

function renderTable(options: {
  selectedRowId?: string;
  rowTone?: (row: Item) => "error" | undefined;
  onSortingChange?: () => void;
}) {
  return render(
    <DataTable
      tableId="items-test"
      ariaLabel="Items"
      columns={columns}
      rows={ITEMS}
      getRowId={(row) => row.id}
      selectedRowId={options.selectedRowId}
      rowTone={options.rowTone}
      onSortingChange={options.onSortingChange}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
    />,
  );
}

function rowOf(text: string): HTMLElement {
  const row = screen.getByText(text).closest('[role="row"]');
  if (!(row instanceof HTMLElement)) {
    throw new Error(`no row for ${text}`);
  }
  return row;
}

it("renders the column icon in the header, before the label, hidden from assistive tech", () => {
  renderTable({});
  const header = screen.getByRole("columnheader", { name: "Name" });
  const icon = header.querySelector("svg");
  expect(icon).not.toBeNull();
  expect(icon).toHaveAttribute("aria-hidden", "true");
  expect(icon).toHaveClass("inline-block", "size-3", "opacity-70");
});

it("renders the column icon inside the sortable header button too", () => {
  renderTable({ onSortingChange: () => undefined });
  const header = screen.getByRole("columnheader", { name: "Name" });
  expect(header.querySelector("button svg")).not.toBeNull();
});

it("marks an error row with the error edge when it is not selected", () => {
  renderTable({ rowTone: (row) => (row.failed ? "error" : undefined) });
  expect(rowOf("broken")).toHaveClass("shadow-[inset_2px_0_0_var(--status-error)]");
  expect(rowOf("fine")).not.toHaveClass("shadow-[inset_2px_0_0_var(--status-error)]");
});

it("keeps the selected style on a selected error row", () => {
  renderTable({
    selectedRowId: "b",
    rowTone: (row) => (row.failed ? "error" : undefined),
  });
  const row = rowOf("broken");
  expect(row).toHaveAttribute("aria-selected", "true");
  expect(row).toHaveClass("bg-selection", "shadow-[inset_2px_0_0_var(--primary)]");
  expect(row).not.toHaveClass("shadow-[inset_2px_0_0_var(--status-error)]");
});

it("adds no error edge without a rowTone", () => {
  renderTable({});
  const grid = screen.getByRole("grid", { name: "Items" });
  expect(within(grid).getByText("broken").closest('[role="row"]')).not.toHaveClass(
    "shadow-[inset_2px_0_0_var(--status-error)]",
  );
});

const reorderColumns: DataTableColumn<Item>[] = [
  { id: "status", header: "Status", size: 100, accessorFn: (row) => row.name, cell: () => "" },
  {
    id: "name",
    header: "Name",
    size: 100,
    accessorFn: (row) => row.name,
    enableSorting: true,
    cell: ({ row }) => row.original.name,
  },
  { id: "cost", header: "Cost", size: 100, accessorFn: (row) => row.name, cell: () => "" },
];

// Header slots in client pixels: a cell's midpoint decides the drop slot.
const SLOTS: Record<string, { left: number; width: number }> = {
  status: { left: 0, width: 100 },
  name: { left: 100, width: 100 },
  cost: { left: 200, width: 100 },
};
const LOCKED = new Set(["status"]);

function rectOf(left: number, width: number): DOMRect {
  return {
    x: left,
    y: 0,
    left,
    right: left + width,
    top: 0,
    bottom: 28,
    width,
    height: 28,
    toJSON: () => ({}),
  };
}

function renderReorder(options: {
  onReorder?: (ids: string[]) => void;
  onSortingChange?: () => void;
  locked?: ReadonlySet<string>;
}) {
  const view = render(
    <DataTable
      tableId="reorder-test"
      ariaLabel="Reorder"
      columns={reorderColumns}
      rows={ITEMS}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
      onSortingChange={options.onSortingChange}
      onReorder={options.onReorder}
      lockedColumnIds={options.locked}
    />,
  );
  const grid = screen.getByRole("grid", { name: "Reorder" });
  for (const cell of grid.querySelectorAll<HTMLElement>("[data-column-id]")) {
    const slot = SLOTS[cell.dataset.columnId ?? ""];
    if (slot) {
      cell.getBoundingClientRect = () => rectOf(slot.left, slot.width);
    }
  }
  const scroller = grid.querySelector<HTMLElement>("[data-scroll-container]");
  if (scroller) {
    scroller.getBoundingClientRect = () => rectOf(0, 1000);
  }
  return view;
}

function headerOf(name: string): HTMLElement {
  return screen.getByRole("columnheader", { name });
}

function press(target: HTMLElement, clientX: number) {
  fireEvent.pointerDown(target, { button: 0, clientX, clientY: 10 });
}

function move(clientX: number, clientY = 10) {
  fireEvent.pointerMove(window, { clientX, clientY });
}

function release(clientX: number) {
  fireEvent.pointerUp(window, { clientX, clientY: 10 });
}

afterEach(() => {
  vi.useRealTimers();
  document.body.style.cursor = "";
});

it("reorders a column dragged and released over another slot", () => {
  vi.useFakeTimers();
  const onReorder = vi.fn();
  renderReorder({ onReorder, locked: LOCKED });
  press(headerOf("Name"), 110);
  move(260);
  release(260);
  expect(onReorder).toHaveBeenCalledWith(["status", "cost", "name"]);
});

it("shows the dragged column and the drop indicator once the pointer moves", () => {
  vi.useFakeTimers();
  renderReorder({ onReorder: () => undefined, locked: LOCKED });
  press(headerOf("Name"), 110);
  expect(headerOf("Name")).not.toHaveClass("bg-muted");
  move(115);
  expect(headerOf("Name")).toHaveClass("bg-muted");
  expect(document.body.style.cursor).toBe("grabbing");
  move(260);
  const grid = screen.getByRole("grid", { name: "Reorder" });
  const indicator = grid.querySelector<HTMLElement>(".w-0\\.5");
  expect(indicator).toHaveStyle({ left: "300px" });
  release(260);
  expect(headerOf("Name")).not.toHaveClass("bg-muted");
  expect(document.body.style.cursor).toBe("");
});

it("drops a column before the locked ids when dragged to the far left", () => {
  vi.useFakeTimers();
  const onReorder = vi.fn();
  renderReorder({ onReorder, locked: LOCKED });
  press(headerOf("Cost"), 250);
  move(0);
  release(0);
  expect(onReorder).toHaveBeenCalledWith(["status", "cost", "name"]);
});

it("does nothing when a locked column is dragged", () => {
  vi.useFakeTimers();
  const onReorder = vi.fn();
  renderReorder({ onReorder, locked: LOCKED });
  press(headerOf("Status"), 50);
  move(260);
  expect(headerOf("Status")).not.toHaveClass("bg-muted");
  release(260);
  expect(onReorder).not.toHaveBeenCalled();
});

it("cancels the drag on Escape", () => {
  vi.useFakeTimers();
  const onReorder = vi.fn();
  renderReorder({ onReorder, locked: LOCKED });
  press(headerOf("Name"), 110);
  move(260);
  fireEvent.keyDown(window, { key: "Escape" });
  expect(headerOf("Name")).not.toHaveClass("bg-muted");
  release(260);
  expect(onReorder).not.toHaveBeenCalled();
});

it("does not sort the column after a drag is released", () => {
  vi.useFakeTimers();
  const onReorder = vi.fn();
  const onSortingChange = vi.fn();
  renderReorder({ onReorder, onSortingChange, locked: LOCKED });
  const button = within(headerOf("Name")).getByRole("button");
  press(button, 110);
  move(260);
  release(260);
  fireEvent.click(button);
  expect(onReorder).toHaveBeenCalledTimes(1);
  expect(onSortingChange).not.toHaveBeenCalled();
});

it("sorts on a quick press and release, without reordering", () => {
  vi.useFakeTimers();
  const onReorder = vi.fn();
  const onSortingChange = vi.fn();
  renderReorder({ onReorder, onSortingChange, locked: LOCKED });
  const button = within(headerOf("Name")).getByRole("button");
  press(button, 110);
  act(() => {
    vi.advanceTimersByTime(100);
  });
  release(110);
  fireEvent.click(button);
  expect(onSortingChange).toHaveBeenCalledWith({ id: "name", desc: true });
  expect(onReorder).not.toHaveBeenCalled();
});

it("still sorts when the pointer moves 4 px or less before release", () => {
  vi.useFakeTimers();
  const onReorder = vi.fn();
  const onSortingChange = vi.fn();
  renderReorder({ onReorder, onSortingChange, locked: LOCKED });
  const button = within(headerOf("Name")).getByRole("button");
  press(button, 110);
  move(113);
  expect(headerOf("Name")).not.toHaveClass("bg-muted");
  release(113);
  fireEvent.click(button);
  expect(onSortingChange).toHaveBeenCalledWith({ id: "name", desc: true });
  expect(onReorder).not.toHaveBeenCalled();
});

it("does not start a column drag from the resize handle", () => {
  vi.useFakeTimers();
  renderReorder({ onReorder: () => undefined, locked: LOCKED });
  const handle = screen.getByRole("separator", { name: "Resize Name" });
  press(handle, 200);
  move(260);
  expect(headerOf("Name")).not.toHaveClass("bg-muted");
  expect(document.body.style.cursor).toBe("");
});

it("does nothing on a drag without an onReorder handler", () => {
  vi.useFakeTimers();
  renderReorder({ locked: LOCKED });
  press(headerOf("Name"), 110);
  move(260);
  expect(headerOf("Name")).not.toHaveClass("bg-muted");
  expect(document.body.style.cursor).toBe("");
});

it("renders only the rows that newly appear when the list scrolls by three rows", () => {
  const cellRenders = vi.fn();
  const many: Item[] = Array.from({ length: 200 }, (_, index) => ({
    id: `r${index}`,
    name: `row ${index}`,
    failed: false,
  }));
  const counted: DataTableColumn<Item>[] = [
    {
      id: "name",
      size: 200,
      header: "Name",
      accessorFn: (row) => row.name,
      cell: ({ row }) => {
        cellRenders(row.original.id);
        return row.original.name;
      },
    },
  ];
  render(
    <DataTable
      tableId="count-test"
      ariaLabel="Count"
      columns={counted}
      rows={many}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
    />,
  );
  const grid = screen.getByRole("grid", { name: "Count" });
  const scroller = grid.querySelector("[data-scroll-container]");
  if (!(scroller instanceof HTMLElement)) {
    throw new Error("missing scroll container");
  }
  const renderedBefore = within(grid).getAllByRole("row").length - 1;
  cellRenders.mockClear();
  scroller.scrollTop = ROW_HEIGHT * 3;
  fireEvent.scroll(scroller);
  const newRenders = cellRenders.mock.calls.length;
  // Scrolling by three rows makes three rows enter the window at the bottom and
  // leaves the top rows mounted (overscan keeps them), so only the entering rows
  // should render their cell. Allow each scrolled row to count twice, plus two
  // rows of margin for the range edges; a memoised row that is not re-rendered
  // keeps this far below the rendered count, which the unmemoised table hits.
  const SCROLLED_ROWS = 3;
  expect(newRenders).toBeLessThanOrEqual(SCROLLED_ROWS * 2 + 2);
  expect(newRenders).toBeLessThan(renderedBefore);
});

it("draws faint row lines behind the virtual rows on the sizing container", () => {
  renderTable({});
  const sizer = rowOf("fine").parentElement;
  expect(sizer?.style.backgroundSize).toBe("100% var(--row-height)");
  expect(sizer?.style.backgroundImage).toContain("linear-gradient");
});
