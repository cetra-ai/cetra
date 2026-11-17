// The plan requires a CSS grid with grid/row/gridcell roles so rows can be
// virtualized. The grid is the single tab stop; arrows move an internal focus
// and Enter activates the focused row.
// biome-ignore-all lint/a11y/useSemanticElements: virtualized CSS grid, not a table
// biome-ignore-all lint/a11y/useFocusableInteractive: one tab stop on the grid
// biome-ignore-all lint/a11y/useKeyWithClickEvents: Enter on the grid activates the row

import {
  type ColumnDef,
  columnSizingFeature,
  flexRender,
  type RowData,
  rowSortingFeature,
  type Row as TableRow,
  tableFeatures,
  useTable,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "cn";
import { ArrowDown, ArrowUp, Loader2, type LucideIcon } from "lucide-react";
import {
  type KeyboardEvent,
  memo,
  type ReactNode,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { moveColumn } from "@/components/data-table/column-layout";
import {
  columnSizesAfterReset,
  columnSizesAfterResize,
  readColumnSizes,
  writeColumnSizes,
} from "@/components/data-table/column-size";
import type { RowAnchor } from "@/lib/list-return";
import { matchesBinding } from "@/lib/shortcuts";

const features = tableFeatures({
  columnSizingFeature,
  rowSortingFeature,
  columnMeta: {} as {
    align?: "left" | "right";
    mono?: boolean;
    icon?: LucideIcon;
    /** Takes the row's remaining width. `size` is its minimum. */
    grow?: boolean;
  },
});

export type DataTableColumn<Row extends RowData> = ColumnDef<typeof features, Row>;

export const ROW_HEIGHT = 28;
const OVERSCAN = 14;
const FETCH_AHEAD = 20;
/** Pointer travel, in px, that turns a press on a header into a column drag. */
const DRAG_START_PX = 4;
const EDGE_PX = 40;
const EDGE_STEP_PX = 12;
/** How long the pointer must rest on a body row before `onRowHover` fires. */
const ROW_HOVER_MS = 100;
const NO_LOCKED: ReadonlySet<string> = new Set();
/** Quiet time, in ms, after the last scroll before the first visible row is reported. */
const ANCHOR_SETTLE_MS = 150;
/** Pages fetched, at most, while looking for the row a restore scrolls to. */
const RESTORE_MAX_PAGES = 5;

interface ReorderDrag {
  id: string;
  /** Left edge of the drop indicator, relative to the header. */
  left: number;
}

export interface DataTableProps<Row extends RowData> {
  tableId: string;
  ariaLabel: string;
  columns: DataTableColumn<Row>[];
  rows: Row[];
  getRowId: (row: Row) => string;
  selectedRowId?: string;
  onActivate: (row: Row) => void;
  onSelectionMove?: (row: Row) => void;
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  fetchNextPage: () => void;
  sorting?: { id: string; desc: boolean };
  onSortingChange?: (next: { id: string; desc: boolean } | null) => void;
  emptyState: ReactNode;
  /** Trace ids that just arrived over the live stream. */
  highlightedRowIds?: ReadonlySet<string>;
  /** Floating control drawn over the top of the scrollport, for the live pill. */
  notice?: ReactNode;
  /** Marks a row, e.g. `"error"` draws a red edge. A selected row keeps its selected style. */
  rowTone?: (row: Row) => "error" | undefined;
  /** Receives the new order of the column ids after a column is dragged to a new place. */
  onReorder?: (ids: string[]) => void;
  /** Columns that cannot be moved, and that columns cannot be dropped before. */
  lockedColumnIds?: ReadonlySet<string>;
  /** Called once the pointer has rested on a body row for ROW_HOVER_MS. */
  onRowHover?: (row: Row) => void;
  /** Reports the first visible row and its offset, once scrolling has settled. */
  onScrollAnchor?: (anchor: RowAnchor) => void;
  /** Scrolls to this row once after mount, fetching pages until it is loaded. */
  restoreAnchor?: RowAnchor;
}

export function DataTable<Row extends RowData>({
  tableId,
  ariaLabel,
  columns,
  rows,
  getRowId,
  selectedRowId,
  onActivate,
  onSelectionMove,
  hasNextPage,
  isFetchingNextPage,
  fetchNextPage,
  sorting,
  onSortingChange,
  emptyState,
  highlightedRowIds,
  notice,
  rowTone,
  onReorder,
  lockedColumnIds = NO_LOCKED,
  onRowHover,
  onScrollAnchor,
  restoreAnchor,
}: DataTableProps<Row>) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const headerRef = useRef<HTMLDivElement>(null);
  const [columnSizing, setColumnSizing] = useState(() => readColumnSizes(tableId));
  const [focusedIndex, setFocusedIndex] = useState<number | null>(null);
  const [reorderDrag, setReorderDrag] = useState<ReorderDrag | null>(null);
  const stopDrag = useRef<(() => void) | null>(null);
  // Set by a drag's release so the click that follows does not sort the column.
  const suppressClick = useRef(false);

  useEffect(() => () => stopDrag.current?.(), []);

  // The latest rows and callbacks, read by the settle timer, which outlives a render.
  const anchorSource = useRef({ rows, getRowId, onScrollAnchor });
  useLayoutEffect(() => {
    anchorSource.current = { rows, getRowId, onScrollAnchor };
  });
  const anchorTimer = useRef<number | undefined>(undefined);
  const reportAnchor = useCallback(() => {
    window.clearTimeout(anchorTimer.current);
    anchorTimer.current = undefined;
    const scroller = scrollRef.current;
    const source = anchorSource.current;
    if (!scroller || !source.onScrollAnchor) {
      return;
    }
    const index = Math.floor(scroller.scrollTop / ROW_HEIGHT);
    const row = source.rows[index];
    if (row === undefined) {
      return;
    }
    source.onScrollAnchor({
      rowId: source.getRowId(row),
      offset: scroller.scrollTop - index * ROW_HEIGHT,
    });
  }, []);
  // A row opened before the timer fires still reports the position it was opened from.
  const flushAnchor = useCallback(() => {
    if (anchorTimer.current !== undefined) {
      reportAnchor();
    }
  }, [reportAnchor]);
  function onScrollSettle() {
    window.clearTimeout(anchorTimer.current);
    anchorTimer.current = window.setTimeout(reportAnchor, ANCHOR_SETTLE_MS);
  }
  useEffect(() => () => window.clearTimeout(anchorTimer.current), []);

  const restoreDone = useRef(false);
  const restoreFetches = useRef(0);
  useEffect(() => {
    if (!restoreAnchor || restoreDone.current) {
      return;
    }
    const index = rows.findIndex((row) => getRowId(row) === restoreAnchor.rowId);
    if (index >= 0) {
      restoreDone.current = true;
      const scroller = scrollRef.current;
      if (scroller) {
        scroller.scrollTop = index * ROW_HEIGHT + restoreAnchor.offset;
        scroller.dispatchEvent(new Event("scroll"));
      }
      return;
    }
    if (!hasNextPage || restoreFetches.current >= RESTORE_MAX_PAGES) {
      restoreDone.current = true;
      return;
    }
    if (isFetchingNextPage) {
      return;
    }
    restoreFetches.current += 1;
    fetchNextPage();
  }, [restoreAnchor, rows, getRowId, hasNextPage, isFetchingNextPage, fetchNextPage]);

  function commit(next: Record<string, number>) {
    setColumnSizing(next);
    writeColumnSizes(tableId, next);
  }

  const table = useTable({
    features,
    columns,
    data: rows,
    defaultColumn: { enableSorting: false },
    manualSorting: true,
    getRowId: (row) => getRowId(row),
    state: {
      columnSizing,
      sorting: sorting ? [{ id: sorting.id, desc: sorting.desc }] : [],
    },
    onColumnSizingChange: (updater) => {
      const next = typeof updater === "function" ? updater(columnSizing) : updater;
      commit(next);
    },
  });

  // Keeps the row click callback's identity stable, so memoised rows skip re-rendering on scroll.
  const onActivateRef = useRef(onActivate);
  useLayoutEffect(() => {
    onActivateRef.current = onActivate;
  }, [onActivate]);
  const onRowClick = useCallback(
    (index: number, row: TableRow<typeof features, Row>) => {
      setFocusedIndex(index);
      flushAnchor();
      onActivateRef.current(row.original);
    },
    [flushAnchor],
  );

  // One timer is enough: the pointer rests on one row at a time.
  const onRowHoverRef = useRef(onRowHover);
  useLayoutEffect(() => {
    onRowHoverRef.current = onRowHover;
  }, [onRowHover]);
  const hoverTimer = useRef<number | undefined>(undefined);
  const cancelRowHover = useCallback(() => {
    window.clearTimeout(hoverTimer.current);
    hoverTimer.current = undefined;
  }, []);
  useEffect(() => cancelRowHover, [cancelRowHover]);
  const onRowEnter = useCallback(
    (row: TableRow<typeof features, Row>) => {
      cancelRowHover();
      if (!onRowHoverRef.current) {
        return;
      }
      const data = row.original;
      hoverTimer.current = window.setTimeout(() => {
        hoverTimer.current = undefined;
        onRowHoverRef.current?.(data);
      }, ROW_HOVER_MS);
    },
    [cancelRowHover],
  );

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: OVERSCAN,
  });

  const virtualRows = virtualizer.getVirtualItems();
  const lastIndex = virtualRows.at(-1)?.index ?? -1;

  useEffect(() => {
    if (!hasNextPage || isFetchingNextPage || rows.length === 0) {
      return;
    }
    if (lastIndex >= rows.length - FETCH_AHEAD) {
      fetchNextPage();
    }
  }, [fetchNextPage, hasNextPage, isFetchingNextPage, lastIndex, rows.length]);

  useEffect(() => {
    if (focusedIndex !== null && focusedIndex >= rows.length) {
      setFocusedIndex(rows.length === 0 ? null : rows.length - 1);
    }
  }, [focusedIndex, rows.length]);

  function moveFocus(next: number) {
    if (rows.length === 0) {
      return;
    }
    const index = Math.min(rows.length - 1, Math.max(0, next));
    if (index === focusedIndex) {
      return;
    }
    setFocusedIndex(index);
    virtualizer.scrollToIndex(index, { align: "auto" });
    if (selectedRowId !== undefined) {
      const row = rows[index];
      if (row) {
        onSelectionMove?.(row);
      }
    }
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (rows.length === 0) {
      return;
    }
    const selectedIndex =
      selectedRowId === undefined ? -1 : rows.findIndex((row) => getRowId(row) === selectedRowId);
    const origin = focusedIndex ?? selectedIndex;
    if (matchesBinding(event, "row-next")) {
      event.preventDefault();
      moveFocus(origin + 1);
    } else if (matchesBinding(event, "row-prev")) {
      event.preventDefault();
      moveFocus(origin - 1);
    } else if (event.key === "Home") {
      event.preventDefault();
      moveFocus(0);
    } else if (event.key === "End") {
      event.preventDefault();
      moveFocus(rows.length - 1);
    } else if (matchesBinding(event, "open") && focusedIndex !== null) {
      event.preventDefault();
      const row = rows[focusedIndex];
      if (row) {
        flushAnchor();
        onActivate(row);
      }
    }
  }

  function beginResize(event: React.PointerEvent<HTMLDivElement>, id: string, size: number) {
    event.preventDefault();
    event.stopPropagation();
    const startX = event.clientX;
    const move = (ev: PointerEvent) => {
      commit(columnSizesAfterResize(columnSizing, id, size + (ev.clientX - startX)));
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      if (stopDrag.current === stop) {
        stopDrag.current = null;
      }
    };
    stopDrag.current?.();
    stopDrag.current = stop;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
  }

  /**
   * Press a header and drag it to a new place. The drag starts once the pointer has
   * moved DRAG_START_PX; a press released before that is a click, so sorting still works.
   */
  function beginReorder(event: React.PointerEvent<HTMLDivElement>, id: string) {
    if (!onReorder || event.button !== 0 || lockedColumnIds.has(id)) {
      return;
    }
    const reorder: (ids: string[]) => void = onReorder;
    const ids = table.getHeaderGroups()[0]?.headers.map((header) => header.column.id) ?? [];
    const startX = event.clientX;
    const startY = event.clientY;
    let dragging = false;
    let cancelled = false;
    let lastX = startX;
    let target: number | undefined;
    let shownLeft: number | undefined;
    let frame: number | undefined;

    // Places the indicator at the boundary of the drop slot under `clientX`.
    function updateDrop(clientX: number) {
      const header = headerRef.current;
      if (!header) {
        return;
      }
      const others = Array.from(header.querySelectorAll<HTMLElement>("[data-column-id]")).filter(
        (cell) => cell.dataset.columnId !== id,
      );
      const boxes = others.map((cell) => cell.getBoundingClientRect());
      const index = boxes.filter((box) => box.left + box.width / 2 < clientX).length;
      const boundary = index < boxes.length ? boxes[index]?.left : boxes.at(-1)?.right;
      if (boundary === undefined) {
        return;
      }
      const left = boundary - header.getBoundingClientRect().left;
      if (index !== target || left !== shownLeft) {
        target = index;
        shownLeft = left;
        setReorderDrag({ id, left });
      }
    }

    // Scrolls the body while the pointer is near its left or right edge.
    function tick() {
      frame = window.requestAnimationFrame(tick);
      const scroller = scrollRef.current;
      if (!scroller) {
        return;
      }
      const box = scroller.getBoundingClientRect();
      if (lastX - box.left < EDGE_PX) {
        scroller.scrollLeft -= EDGE_STEP_PX;
      } else if (box.right - lastX < EDGE_PX) {
        scroller.scrollLeft += EDGE_STEP_PX;
      }
      if (headerRef.current) {
        headerRef.current.scrollLeft = scroller.scrollLeft;
      }
      updateDrop(lastX);
    }

    function startDrag() {
      dragging = true;
      document.body.style.cursor = "grabbing";
      window.addEventListener("keydown", onKey);
      frame = window.requestAnimationFrame(tick);
      updateDrop(lastX);
    }

    // Ends the drag's motion and visuals; the release listener stays until pointerup.
    function leaveDrag() {
      if (frame !== undefined) {
        window.cancelAnimationFrame(frame);
        frame = undefined;
      }
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("keydown", onKey);
      document.body.style.cursor = "";
      setReorderDrag(null);
    }

    function stop() {
      leaveDrag();
      window.removeEventListener("pointerup", onUp);
      if (stopDrag.current === stop) {
        stopDrag.current = null;
      }
    }

    function onMove(ev: globalThis.PointerEvent) {
      lastX = ev.clientX;
      if (dragging) {
        updateDrop(lastX);
        return;
      }
      if (Math.hypot(ev.clientX - startX, ev.clientY - startY) > DRAG_START_PX) {
        startDrag();
      }
    }

    function onKey(ev: globalThis.KeyboardEvent) {
      if (ev.key === "Escape") {
        cancelled = true;
        leaveDrag();
      }
    }

    function onUp() {
      if (dragging) {
        suppressClick.current = true;
        window.setTimeout(() => {
          suppressClick.current = false;
        }, 0);
        const next =
          cancelled || target === undefined
            ? undefined
            : moveColumn(ids, id, target, lockedColumnIds);
        if (next) {
          reorder(next);
        }
      }
      stop();
    }

    stopDrag.current?.();
    stopDrag.current = stop;
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  const headerGroup = table.getHeaderGroups()[0];
  const template = headerGroup
    ? headerGroup.headers
        .map((header) =>
          header.column.columnDef.meta?.grow
            ? `minmax(${header.getSize()}px, 1fr)`
            : `${header.getSize()}px`,
        )
        .join(" ")
    : undefined;
  const totalWidth = headerGroup
    ? headerGroup.headers.reduce((sum, header) => sum + header.getSize(), 0)
    : undefined;
  const model = table.getRowModel().rows;

  return (
    <div
      role="grid"
      aria-label={ariaLabel}
      aria-rowcount={rows.length + 1 + (isFetchingNextPage ? 1 : 0)}
      aria-colcount={columns.length}
      tabIndex={0}
      className="flex min-h-0 flex-1 flex-col outline-none focus-visible:outline-2 focus-visible:outline-ring focus-visible:-outline-offset-2 focus-visible:outline-solid"
      onKeyDown={onKeyDown}
    >
      {/* The highlight keyframes, for `highlightedRowIds`, on a page without the live toggle. */}
      {headerGroup ? (
        <div
          ref={headerRef}
          className="relative flex shrink-0 overflow-hidden [scrollbar-gutter:stable]"
        >
          {reorderDrag ? (
            <div
              aria-hidden
              className="pointer-events-none absolute inset-y-0 z-20 w-0.5 bg-primary"
              style={{ left: reorderDrag.left }}
            />
          ) : null}
          <div
            role="row"
            aria-rowindex={1}
            className="grid h-row shrink-0 items-center border-b bg-surface select-none text-[11px] font-medium tracking-wide text-muted-foreground uppercase"
            style={{ gridTemplateColumns: template, minWidth: totalWidth }}
          >
            {headerGroup.headers.map((header) => {
              const sorted = header.column.getIsSorted();
              const canSort = header.column.getCanSort();
              const label =
                typeof header.column.columnDef.header === "string"
                  ? header.column.columnDef.header
                  : "column";
              const HeaderIcon = header.column.columnDef.meta?.icon;
              return (
                <div
                  key={header.id}
                  role="columnheader"
                  data-column-id={header.column.id}
                  onPointerDown={(event) => beginReorder(event, header.column.id)}
                  aria-labelledby={`${tableId}-${header.id}-label`}
                  aria-sort={
                    canSort
                      ? sorted === "asc"
                        ? "ascending"
                        : sorted === "desc"
                          ? "descending"
                          : "none"
                      : undefined
                  }
                  className={cn(
                    "relative flex h-full min-w-0 items-center px-2",
                    header.column.columnDef.meta?.align === "right" && "justify-end",
                    reorderDrag?.id === header.column.id && "bg-muted",
                  )}
                >
                  {canSort && onSortingChange ? (
                    <button
                      type="button"
                      id={`${tableId}-${header.id}-label`}
                      className="inline-flex min-w-0 items-center gap-1 uppercase hover:text-foreground"
                      onClick={() => {
                        if (suppressClick.current) {
                          suppressClick.current = false;
                          return;
                        }
                        if (sorting?.id !== header.column.id) {
                          onSortingChange({ id: header.column.id, desc: true });
                          return;
                        }
                        if (sorting.desc) {
                          onSortingChange({ id: header.column.id, desc: false });
                          return;
                        }
                        onSortingChange(null);
                      }}
                    >
                      <span className="truncate">
                        {HeaderIcon ? (
                          <HeaderIcon
                            aria-hidden
                            className="mr-1 inline-block size-3 align-[-2px] opacity-70"
                          />
                        ) : null}
                        {flexRender(header.column.columnDef.header, header.getContext())}
                      </span>
                      <SortArrow direction={sorted} />
                    </button>
                  ) : (
                    <span id={`${tableId}-${header.id}-label`} className="min-w-0 truncate">
                      {HeaderIcon ? (
                        <HeaderIcon
                          aria-hidden
                          className="mr-1 inline-block size-3 align-[-2px] opacity-70"
                        />
                      ) : null}
                      {flexRender(header.column.columnDef.header, header.getContext())}
                    </span>
                  )}
                  <div
                    role="separator"
                    aria-orientation="vertical"
                    aria-label={`Resize ${label}`}
                    className="absolute inset-y-0 -right-1 z-10 w-2 cursor-col-resize after:absolute after:inset-y-1 after:left-1/2 after:w-px after:-translate-x-1/2 after:bg-transparent hover:after:bg-primary active:after:bg-primary"
                    onPointerDown={(event) =>
                      beginResize(event, header.column.id, header.getSize())
                    }
                    onDoubleClick={(event) => {
                      event.preventDefault();
                      event.stopPropagation();
                      commit(columnSizesAfterReset(columnSizing, header.column.id));
                    }}
                  />
                </div>
              );
            })}
          </div>
          {/* Lets the header scroll as far as the body, whose overflow-auto can scroll one gutter further. */}
          <div aria-hidden="true" className="w-8 shrink-0 border-b bg-surface" />
        </div>
      ) : null}
      <div
        ref={scrollRef}
        data-scroll-container
        className="relative min-h-0 flex-1 overflow-auto [scrollbar-gutter:stable]"
        onScroll={(event) => {
          if (headerRef.current) {
            headerRef.current.scrollLeft = event.currentTarget.scrollLeft;
          }
          onScrollSettle();
        }}
      >
        {notice ? (
          <div className="sticky top-2 z-10 flex h-0 justify-center">
            <div className="pointer-events-auto">{notice}</div>
          </div>
        ) : null}
        {rows.length === 0 ? (
          emptyState
        ) : (
          <div
            className="relative"
            style={{
              height: virtualizer.getTotalSize() + (isFetchingNextPage ? ROW_HEIGHT : 0),
              // Faint row lines show where rows are not rendered yet, so blank space looks like empty rows.
              backgroundImage:
                "linear-gradient(to bottom, transparent calc(var(--row-height) - 1px), color-mix(in oklab, var(--border) 60%, transparent) 0)",
              backgroundSize: "100% var(--row-height)",
            }}
          >
            {virtualRows.map((item) => {
              const row = model[item.index];
              if (!row) {
                return null;
              }
              const selected = row.id === selectedRowId;
              return (
                <BodyRow
                  key={row.id}
                  row={row}
                  index={item.index}
                  start={item.start}
                  selected={selected}
                  focused={focusedIndex === item.index}
                  highlighted={highlightedRowIds?.has(row.id) ?? false}
                  tone={!selected && rowTone?.(row.original) === "error" ? "error" : undefined}
                  template={template}
                  totalWidth={totalWidth}
                  onRowClick={onRowClick}
                  onRowEnter={onRowEnter}
                  onRowLeave={cancelRowHover}
                />
              );
            })}
            {isFetchingNextPage ? (
              <div
                role="row"
                aria-rowindex={rows.length + 2}
                className="absolute right-0 left-0 flex items-center px-2 text-muted-foreground"
                style={{ top: virtualizer.getTotalSize(), height: ROW_HEIGHT }}
              >
                <Loader2 className="size-3 animate-spin" />
                <span className="sr-only">Loading more</span>
              </div>
            ) : null}
          </div>
        )}
      </div>
    </div>
  );
}

interface BodyRowProps<TData extends RowData> {
  row: TableRow<typeof features, TData>;
  index: number;
  start: number;
  selected: boolean;
  focused: boolean;
  highlighted: boolean;
  tone: "error" | undefined;
  template: string | undefined;
  totalWidth: number | undefined;
  onRowClick: (index: number, row: TableRow<typeof features, TData>) => void;
  onRowEnter: (row: TableRow<typeof features, TData>) => void;
  onRowLeave: () => void;
}

// One virtual body row. Every prop is a primitive or a stable reference, so a scroll
// that only changes the visible range re-renders only the rows that newly appear.
function BodyRowView<TData extends RowData>({
  row,
  index,
  start,
  selected,
  focused,
  highlighted,
  tone,
  template,
  totalWidth,
  onRowClick,
  onRowEnter,
  onRowLeave,
}: BodyRowProps<TData>) {
  return (
    <div
      role="row"
      aria-rowindex={index + 2}
      aria-selected={selected}
      data-focused={focused ? "" : undefined}
      className={cn(
        "absolute top-0 grid w-full items-center border-b border-border/60",
        selected ? "bg-selection shadow-[inset_2px_0_0_var(--primary)]" : "hover:bg-row-hover",
        focused && !selected && "bg-row-hover",
        tone === "error" && "shadow-[inset_2px_0_0_var(--status-error)]",
        highlighted && "live-row",
      )}
      style={{
        height: ROW_HEIGHT,
        transform: `translateY(${start}px)`,
        gridTemplateColumns: template,
        minWidth: totalWidth,
      }}
      onClick={() => onRowClick(index, row)}
      onPointerEnter={() => onRowEnter(row)}
      onPointerLeave={onRowLeave}
    >
      {row.getAllCells().map((cell) => (
        <div
          key={cell.id}
          role="gridcell"
          className={cn(
            "min-w-0 truncate px-2",
            cell.column.columnDef.meta?.align === "right" && "text-right",
            cell.column.columnDef.meta?.mono && "font-mono",
          )}
        >
          {flexRender(cell.column.columnDef.cell, cell.getContext())}
        </div>
      ))}
    </div>
  );
}

const BodyRow = memo(BodyRowView) as typeof BodyRowView;

function SortArrow({ direction }: { direction: false | "asc" | "desc" }) {
  if (direction === "asc") {
    return <ArrowUp aria-hidden className="size-3 shrink-0" />;
  }
  if (direction === "desc") {
    return <ArrowDown aria-hidden className="size-3 shrink-0" />;
  }
  return null;
}
