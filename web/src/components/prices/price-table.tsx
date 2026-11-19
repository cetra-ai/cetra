import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { SettingsTableFrame } from "@/components/settings/settings-list";
import { escapeRegex, formatUnitPrice, isAlwaysValid } from "@/lib/prices";

type PriceRow = Schema<"ModelPrice">;

export interface PriceSorting {
  id: string;
  desc: boolean;
}

/** Server paging of a list: the rest loads as the user scrolls. */
export interface Paging {
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  fetchNextPage: () => unknown;
}

const noop = () => undefined;

/** Built-in rows have no id; their match pattern is unique. */
function rowId(row: PriceRow): string {
  return row.id ?? `builtin:${row.match_pattern}`;
}

const PRICE_COLUMNS: DataTableColumn<PriceRow>[] = [
  {
    id: "model",
    accessorFn: (row) => row.model_name,
    enableSorting: true,
    size: 240,
    header: "Model",
    meta: { grow: true },
    cell: ({ row }) => (
      // One line: rows have the fixed table row height, so the pattern follows the name.
      <div className="flex min-w-0 items-baseline gap-2">
        <span className="shrink-0 truncate">{row.original.model_name}</span>
        {row.original.match_pattern !== escapeRegex(row.original.model_name) ? (
          <span className="min-w-0 truncate font-mono text-caption text-muted-foreground">
            {row.original.match_pattern}
          </span>
        ) : null}
      </div>
    ),
  },
  {
    id: "provider",
    size: 96,
    header: "Provider",
    cell: ({ row }) => row.original.provider ?? "Any",
  },
  {
    id: "input",
    accessorFn: (row) => row.price_input,
    enableSorting: true,
    size: 88,
    header: "Input",
    meta: { align: "right" },
    cell: ({ row }) => formatUnitPrice(row.original.price_input),
  },
  {
    id: "output",
    accessorFn: (row) => row.price_output,
    enableSorting: true,
    size: 88,
    header: "Output",
    meta: { align: "right" },
    cell: ({ row }) => formatUnitPrice(row.original.price_output),
  },
  {
    id: "cache_read",
    size: 96,
    header: "Cache read",
    meta: { align: "right" },
    enableSorting: false,
    cell: ({ row }) => formatUnitPrice(row.original.price_cache_read),
  },
  {
    id: "cache_write",
    size: 96,
    header: "Cache write",
    meta: { align: "right" },
    enableSorting: false,
    cell: ({ row }) => formatUnitPrice(row.original.price_cache_write),
  },
  {
    id: "valid_from",
    size: 104,
    header: "Valid from",
    enableSorting: false,
    cell: ({ row }) =>
      isAlwaysValid(row.original.valid_from) ? "Always" : row.original.valid_from.slice(0, 10),
  },
  {
    id: "tiers",
    size: 64,
    header: "Tiers",
    meta: { align: "right" },
    enableSorting: false,
    cell: ({ row }) => String(row.original.tiers.length || "—"),
  },
];

/**
 * A price list. The rows are sorted by the caller (`sorting`, `onSortingChange`) only when every
 * row is loaded; a paged list passes `more` and no sorting.
 */
export function PriceTable({
  label,
  rows,
  selectedRowId,
  onActivate,
  sorting,
  onSortingChange,
  more,
}: {
  label: string;
  rows: PriceRow[];
  selectedRowId?: string;
  /** Opens a row. Absent for a read-only list. */
  onActivate?: (row: PriceRow) => void;
  sorting?: PriceSorting;
  onSortingChange?: (next: PriceSorting | null) => void;
  more?: Paging;
}) {
  return (
    <SettingsTableFrame rowCount={rows.length}>
      <DataTable
        tableId={label.toLowerCase().replaceAll(" ", "-")}
        ariaLabel={label}
        columns={PRICE_COLUMNS}
        rows={rows}
        getRowId={rowId}
        selectedRowId={selectedRowId}
        onActivate={onActivate ?? noop}
        sorting={sorting}
        onSortingChange={onSortingChange}
        hasNextPage={more?.hasNextPage ?? false}
        isFetchingNextPage={more?.isFetchingNextPage ?? false}
        fetchNextPage={more?.fetchNextPage ?? noop}
        emptyState={null}
      />
    </SettingsTableFrame>
  );
}
