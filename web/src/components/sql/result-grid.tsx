import {
  Braces,
  Calendar,
  Clock,
  Hash,
  type LucideIcon,
  Rows3,
  ToggleLeft,
  Type,
} from "lucide-react";
import { memo, useMemo, useState } from "react";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { EmptyState } from "@/components/empty-state";
import { RowDetailSheet } from "@/components/sql/row-detail";
import { sortKindOf, sortSqlRows } from "@/lib/sql/sort";
import {
  isBooleanType,
  isDateType,
  isNestedType,
  isNumericType,
  isTextType,
  isTimestampType,
} from "@/lib/sql/types";
import { FIT_ROWS, fitWidth, formatSqlCell } from "@/lib/sql/widths";

export const RESULT_TABLE_ID = "sql-results";

export type SqlRow = { index: number; values: Record<string, unknown> };

type SqlResult = Schema<"SqlQueryResult">;
type SqlSort = { id: string; desc: boolean };

/** Sort state and the open row, kept with the result they belong to: a new result starts clean. */
interface GridView {
  result: SqlResult;
  sort: SqlSort | null;
  openIndex: number | null;
}

// Column ids are `c<index>`: DataTable builds DOM ids from them, so names with spaces are safe.
function sqlColumnId(index: number): string {
  return `c${index}`;
}

/** Own keys only: a column named `__proto__` or `constructor` must not read the prototype. */
function sqlCellValue(values: Record<string, unknown>, name: string): unknown {
  return Object.hasOwn(values, name) ? values[name] : undefined;
}

/** The icon that stands for a column type in the column header. */
function sqlTypeIcon(type: string): LucideIcon | undefined {
  if (isNumericType(type)) {
    return Hash;
  }
  if (isTimestampType(type)) {
    return Clock;
  }
  if (isDateType(type)) {
    return Calendar;
  }
  if (isTextType(type)) {
    return Type;
  }
  if (isBooleanType(type)) {
    return ToggleLeft;
  }
  if (isNestedType(type)) {
    return Braces;
  }
  return undefined;
}

function sqlRows(result: SqlResult): SqlRow[] {
  return result.rows.map((values, index) => ({
    index,
    values: values as Record<string, unknown>,
  }));
}

export function sqlColumns(result: SqlResult): DataTableColumn<SqlRow>[] {
  const rows = sqlRows(result);
  const lastIndex = result.columns.length - 1;
  return result.columns.map((column, index) => ({
    id: sqlColumnId(index),
    // TanStack only treats a column as sortable when it has an accessor.
    accessorFn: (row) => sqlCellValue(row.values, column.name),
    size: fitWidth(
      column.name,
      rows.slice(0, FIT_ROWS).map((row) => sqlCellValue(row.values, column.name)),
    ),
    // A render function, so the column type shows as a tooltip; its accessible name is `label`.
    header: () => <span title={column.type}>{column.name}</span>,
    enableSorting: true,
    meta: {
      label: column.name,
      mono: true,
      align: isNumericType(column.type) ? "right" : "left",
      icon: sqlTypeIcon(column.type),
      grow: index === lastIndex,
    },
    cell: ({ row }) => {
      const value = sqlCellValue(row.original.values, column.name);
      const text = formatSqlCell(value);
      // Null, missing, objects and arrays read muted, so structured values stand apart.
      const muted = value === undefined || typeof value === "object";
      return (
        <span title={text} className={muted ? "text-muted-foreground" : undefined}>
          {text}
        </span>
      );
    },
  }));
}

export const SqlResultGrid = memo(function SqlResultGrid({ result }: { result: SqlResult }) {
  const [saved, setSaved] = useState<GridView | null>(null);
  const view = saved?.result === result ? saved : null;
  const sort = view?.sort ?? null;
  const openIndex = view?.openIndex ?? null;

  function update(next: { sort?: SqlSort | null; openIndex?: number | null }) {
    setSaved({ result, sort, openIndex, ...next });
  }

  const columns = useMemo(() => sqlColumns(result), [result]);
  const rows = useMemo(() => sqlRows(result), [result]);
  const sortedRows = useMemo(() => {
    const column = result.columns.find((_, index) => sqlColumnId(index) === sort?.id);
    if (!sort || !column) {
      return rows;
    }
    return sortSqlRows(
      rows,
      (row) => sqlCellValue(row.values, column.name),
      sortKindOf(column.type),
      sort.desc,
    );
  }, [result, rows, sort]);
  // `index` is the position in the result, so rows[openIndex] is the open row.
  const openRow = openIndex === null ? undefined : rows[openIndex];

  return (
    <>
      <DataTable
        tableId={RESULT_TABLE_ID}
        ariaLabel="Query results"
        columns={columns}
        rows={sortedRows}
        getRowId={(row) => String(row.index)}
        selectedRowId={openRow ? String(openRow.index) : undefined}
        onActivate={(row) => update({ openIndex: row.index })}
        sorting={sort ?? undefined}
        onSortingChange={(next) => update({ sort: next })}
        hasNextPage={false}
        isFetchingNextPage={false}
        fetchNextPage={() => {}}
        emptyState={<EmptyState icon={Rows3} title="No rows" />}
      />
      <RowDetailSheet
        row={openRow}
        columns={result.columns}
        onClose={() => update({ openIndex: null })}
      />
    </>
  );
});
