import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { Clock } from "lucide-react";
import { afterEach, expect, it, vi } from "vitest";
import { DataTable, type DataTableColumn, ROW_HEIGHT } from "@/components/data-table/data-table";
import { DENSITY_STORAGE_KEY, ThemeProvider } from "@/components/theme-provider";

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

it("lets a grow column take the remaining width, with its size as the minimum", () => {
  const growing: DataTableColumn<Item>[] = [
    { id: "name", size: 200, header: "Name", accessorFn: (row) => row.name, cell: () => "" },
    {
      id: "text",
      size: 360,
      header: "Text",
      accessorFn: (row) => row.name,
      meta: { grow: true },
      cell: () => "",
    },
  ];
  render(
    <DataTable
      tableId="grow-test"
      ariaLabel="Grow"
      columns={growing}
      rows={ITEMS}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
    />,
  );
  const header = screen
    .getByRole("grid", { name: "Grow" })
    .querySelector<HTMLElement>('[aria-rowindex="1"]');
  expect(header?.style.gridTemplateColumns).toBe("200px minmax(360px, 1fr)");
  expect(header?.style.minWidth).toBe("560px");
});

it("keeps fixed pixel widths when no column grows", () => {
  renderTable({});
  const header = screen
    .getByRole("grid", { name: "Items" })
    .querySelector<HTMLElement>('[aria-rowindex="1"]');
  expect(header?.style.gridTemplateColumns).toBe("200px");
});

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

it("calls onRowHover with the row once the pointer has rested on it for 100 ms", () => {
  vi.useFakeTimers();
  const onRowHover = vi.fn();
  render(
    <DataTable
      tableId="hover-test"
      ariaLabel="Hover"
      columns={columns}
      rows={ITEMS}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
      onRowHover={onRowHover}
    />,
  );
  fireEvent.pointerEnter(rowOf("broken"));
  act(() => {
    vi.advanceTimersByTime(99);
  });
  expect(onRowHover).not.toHaveBeenCalled();
  act(() => {
    vi.advanceTimersByTime(1);
  });
  expect(onRowHover).toHaveBeenCalledTimes(1);
  expect(onRowHover).toHaveBeenCalledWith(ITEMS[1]);
});

it("does not call onRowHover when the pointer leaves before 100 ms", () => {
  vi.useFakeTimers();
  const onRowHover = vi.fn();
  render(
    <DataTable
      tableId="hover-leave-test"
      ariaLabel="Hover"
      columns={columns}
      rows={ITEMS}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
      onRowHover={onRowHover}
    />,
  );
  fireEvent.pointerEnter(rowOf("fine"));
  act(() => {
    vi.advanceTimersByTime(50);
  });
  fireEvent.pointerLeave(rowOf("fine"));
  act(() => {
    vi.advanceTimersByTime(200);
  });
  expect(onRowHover).not.toHaveBeenCalled();
});

it("cancels a pending hover when the table unmounts", () => {
  vi.useFakeTimers();
  const onRowHover = vi.fn();
  const view = render(
    <DataTable
      tableId="hover-unmount-test"
      ariaLabel="Hover"
      columns={columns}
      rows={ITEMS}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
      onRowHover={onRowHover}
    />,
  );
  fireEvent.pointerEnter(rowOf("fine"));
  view.unmount();
  act(() => {
    vi.advanceTimersByTime(200);
  });
  expect(onRowHover).not.toHaveBeenCalled();
});

function manyRows(count: number): Item[] {
  return Array.from({ length: count }, (_, index) => ({
    id: `r${index}`,
    name: `row ${index}`,
    failed: false,
  }));
}

function scrollerOf(grid: HTMLElement): HTMLElement {
  const scroller = grid.querySelector<HTMLElement>("[data-scroll-container]");
  if (!(scroller instanceof HTMLElement)) {
    throw new Error("missing scroll container");
  }
  return scroller;
}

it("reports the first visible row and its offset once scrolling settles", () => {
  vi.useFakeTimers();
  try {
    const onScrollAnchor = vi.fn();
    render(
      <DataTable
        tableId="anchor-test"
        ariaLabel="Anchor"
        columns={columns}
        rows={manyRows(200)}
        getRowId={(row) => row.id}
        onActivate={() => undefined}
        hasNextPage={false}
        isFetchingNextPage={false}
        fetchNextPage={() => undefined}
        emptyState={null}
        onScrollAnchor={onScrollAnchor}
      />,
    );
    const scroller = scrollerOf(screen.getByRole("grid", { name: "Anchor" }));
    scroller.scrollTop = ROW_HEIGHT * 3 + 10;
    fireEvent.scroll(scroller);
    act(() => {
      vi.advanceTimersByTime(149);
    });
    expect(onScrollAnchor).not.toHaveBeenCalled();
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(onScrollAnchor).toHaveBeenCalledTimes(1);
    expect(onScrollAnchor).toHaveBeenCalledWith({ rowId: "r3", offset: 10 });
  } finally {
    vi.useRealTimers();
  }
});

it("scrolls to the restore row at its index times the row height plus the offset", () => {
  render(
    <DataTable
      tableId="restore-test"
      ariaLabel="Restore"
      columns={columns}
      rows={manyRows(200)}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
      restoreAnchor={{ rowId: "r50", offset: 5 }}
    />,
  );
  expect(scrollerOf(screen.getByRole("grid", { name: "Restore" })).scrollTop).toBe(
    50 * ROW_HEIGHT + 5,
  );
});

it("waits for the first page when the table mounts empty, then restores", () => {
  const table = (rows: Item[]) => (
    <DataTable
      tableId="restore-late-test"
      ariaLabel="Late"
      columns={columns}
      rows={rows}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
      restoreAnchor={{ rowId: "r50", offset: 5 }}
    />
  );
  // A return refetches the list: the table first renders with no rows and no next page.
  const view = render(table([]));
  view.rerender(table(manyRows(200)));
  expect(scrollerOf(screen.getByRole("grid", { name: "Late" })).scrollTop).toBe(
    50 * ROW_HEIGHT + 5,
  );
});

it("fetches at most twenty more pages for the restore row, then stops at the top", () => {
  const fetchNextPage = vi.fn();
  const table = (rows: Item[]) => (
    <DataTable
      tableId="restore-pages-test"
      ariaLabel="Pages"
      columns={columns}
      rows={rows}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage
      isFetchingNextPage={false}
      fetchNextPage={fetchNextPage}
      emptyState={null}
      restoreAnchor={{ rowId: "missing", offset: 0 }}
    />
  );
  const view = render(table(manyRows(100)));
  expect(fetchNextPage).toHaveBeenCalledTimes(1);
  for (let page = 2; page <= 21; page += 1) {
    view.rerender(table(manyRows(100 * page)));
  }
  expect(fetchNextPage).toHaveBeenCalledTimes(20);
  expect(scrollerOf(screen.getByRole("grid", { name: "Pages" })).scrollTop).toBe(0);
});

it("loads a screen of rows below a restore row near the end before scrolling to it", () => {
  const fetchNextPage = vi.fn();
  const clientHeight = vi
    .spyOn(HTMLElement.prototype, "clientHeight", "get")
    .mockReturnValue(10 * ROW_HEIGHT);
  try {
    const table = (rows: Item[], hasNextPage: boolean) => (
      <DataTable
        tableId="restore-end-test"
        ariaLabel="End"
        columns={columns}
        rows={rows}
        getRowId={(row) => row.id}
        onActivate={() => undefined}
        hasNextPage={hasNextPage}
        isFetchingNextPage={false}
        fetchNextPage={fetchNextPage}
        emptyState={null}
        restoreAnchor={{ rowId: "r95", offset: 4 }}
      />
    );
    // r95 is loaded, but only 5 rows follow it: scrolling now would be clamped.
    const view = render(table(manyRows(100), true));
    expect(fetchNextPage).toHaveBeenCalledTimes(1);
    view.rerender(table(manyRows(200), false));
    expect(scrollerOf(screen.getByRole("grid", { name: "End" })).scrollTop).toBe(
      95 * ROW_HEIGHT + 4,
    );
  } finally {
    clientHeight.mockRestore();
  }
});

it("names the resize handle by meta.label when the header is a render function", () => {
  const rendered: DataTableColumn<Item>[] = [
    {
      id: "name",
      size: 200,
      header: () => <span title="Utf8">Name</span>,
      accessorFn: (row) => row.name,
      meta: { label: "Name" },
      cell: ({ row }) => row.original.name,
    },
  ];
  render(
    <DataTable
      tableId="label-test"
      ariaLabel="Labelled"
      columns={rendered}
      rows={ITEMS}
      getRowId={(row) => row.id}
      onActivate={() => undefined}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={null}
    />,
  );
  expect(screen.getByRole("separator", { name: "Resize Name" })).toBeInTheDocument();
});

it("spaces body rows by the comfortable row height when that density is set", () => {
  localStorage.setItem(DENSITY_STORAGE_KEY, "comfortable");
  render(
    <ThemeProvider>
      <DataTable
        tableId="items-comfortable-test"
        ariaLabel="Items"
        columns={columns}
        rows={ITEMS}
        getRowId={(row) => row.id}
        onActivate={() => undefined}
        hasNextPage={false}
        isFetchingNextPage={false}
        fetchNextPage={() => undefined}
        emptyState={null}
      />
    </ThemeProvider>,
  );
  expect(rowOf("broken").style.transform).toBe("translateY(32px)");
  expect(rowOf("broken").style.height).toBe("32px");
  expect(rowOf("fine").style.transform).toBe("translateY(0px)");
});
