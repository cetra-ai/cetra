import { render, screen, within } from "@testing-library/react";
import { Clock } from "lucide-react";
import { expect, it } from "vitest";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";

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
