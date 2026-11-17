import { Search, X } from "lucide-react";
import { type ReactNode, useEffect } from "react";
import { useRowHeight } from "@/components/theme-provider";
import { SplitPane } from "@/components/trace/split-pane";
import { Button } from "@/components/ui/button";
import { InputGroup, InputGroupAddon, InputGroupInput } from "@/components/ui/input-group";

// Same overlays as the command palette's Esc handling: an open dialog, menu or popover keeps Esc.
const OVERLAY_SELECTOR =
  "[role='dialog'][data-state='open'], [role='menu'][data-state='open'], [data-slot='popover-content'][data-state='open']";

function isOverlayOpen(): boolean {
  return document.querySelector(OVERLAY_SELECTOR) !== null;
}

/** Text entry keeps Esc for itself, for example to clear a search box. */
function isTextEntry(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) {
    return false;
  }
  if (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement
  ) {
    return true;
  }
  const editable = target.closest("[contenteditable]");
  return (
    target.isContentEditable ||
    (editable !== null && editable.getAttribute("contenteditable") !== "false")
  );
}

/**
 * The list of a settings page on the left, and its row panel as a drawer on the right.
 * While `open`, Esc calls `onClose`, except inside text entry or over an open overlay.
 */
export function SettingsListLayout({
  open,
  onClose,
  children,
  panel,
}: {
  open: boolean;
  onClose: () => void;
  children: ReactNode;
  panel: ReactNode;
}) {
  useEffect(() => {
    if (!open) {
      return;
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (
        event.key !== "Escape" ||
        event.defaultPrevented ||
        isTextEntry(event.target) ||
        isOverlayOpen()
      ) {
        return;
      }
      onClose();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open, onClose]);

  return (
    <div className="relative h-full min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane name="settings" mode="drawer" open={open} start={children} end={panel} />
      </div>
    </div>
  );
}

/** The right-hand drawer's content: a header with a close button, a body and an optional footer. */
export function SettingsPanel({
  title,
  subtitle,
  onClose,
  children,
  footer,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
}) {
  return (
    <div className="flex h-full min-h-0 flex-col bg-background">
      <div className="flex items-start gap-2 border-b px-4 py-3">
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <h2 className="text-title font-medium">{title}</h2>
          {subtitle ? <div className="text-body text-muted-foreground">{subtitle}</div> : null}
        </div>
        <Button variant="ghost" size="icon-sm" aria-label="Close panel" onClick={onClose}>
          <X aria-hidden="true" />
        </Button>
      </div>
      <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-auto px-4 py-4">{children}</div>
      {footer ? (
        <div className="flex items-center justify-end gap-2 border-t px-4 py-3">{footer}</div>
      ) : null}
    </div>
  );
}

/** Search box above a settings list, with the row count and filters on the right. */
export function SettingsListToolbar({
  label,
  query,
  onQueryChange,
  count,
  children,
}: {
  /** For example `API keys`: the search box reads `Search API keys`. */
  label: string;
  query: string;
  onQueryChange: (next: string) => void;
  /** For example `3 keys`. */
  count?: string;
  /** Filters, on the right. */
  children?: ReactNode;
}) {
  return (
    <div className="flex items-center gap-3">
      <InputGroup className="h-7 max-w-xs">
        <InputGroupAddon align="inline-start">
          <Search aria-hidden="true" />
        </InputGroupAddon>
        <InputGroupInput
          type="search"
          aria-label={`Search ${label}`}
          placeholder={`Search ${label}`}
          value={query}
          className="h-7 text-body"
          onChange={(event) => {
            onQueryChange(event.target.value);
          }}
        />
      </InputGroup>
      {count ? <span className="text-body text-muted-foreground">{count}</span> : null}
      {children ? <div className="ml-auto flex items-center gap-2">{children}</div> : null}
    </div>
  );
}

const MIN_VISIBLE_ROWS = 3;
const MAX_VISIBLE_ROWS = 12;
/** Room for the horizontal scrollbar a narrow window shows, so it never hides the last row. */
const SCROLLBAR_ALLOWANCE_PX = 12;

/**
 * Bordered box for a DataTable. It is as tall as the rows (3 to 12, plus the header), so short
 * lists show no empty space and long ones scroll inside the table.
 */
export function SettingsTableFrame({
  rowCount,
  children,
}: {
  rowCount: number;
  children: ReactNode;
}) {
  const rowHeight = useRowHeight();
  const visible = Math.min(MAX_VISIBLE_ROWS, Math.max(MIN_VISIBLE_ROWS, rowCount));
  return (
    <div
      className="flex flex-col overflow-hidden rounded-lg border"
      style={{ height: `${(visible + 1) * rowHeight + 2 + SCROLLBAR_ALLOWANCE_PX}px` }}
    >
      {children}
    </div>
  );
}

/** Trimmed, case-insensitive substring match over every text value of a row. */
export function filterRows<Row>(
  rows: Row[],
  query: string | undefined,
  text: (row: Row) => (string | undefined)[],
): Row[] {
  const needle = query?.trim().toLowerCase() ?? "";
  if (needle === "") {
    return rows;
  }
  return rows.filter((row) => text(row).some((value) => value?.toLowerCase().includes(needle)));
}

export type SortValue = string | number | undefined;

const TEXT_ORDER: Intl.CollatorOptions = { sensitivity: "base", numeric: true };

/**
 * Sorts by the column `sorting.id` through `keys`. Returns a new array. An unknown column or no
 * sorting keeps the input order. Undefined values sort last in both directions. The sort is stable.
 */
export function sortRows<Row>(
  rows: Row[],
  sorting: { id: string; desc: boolean } | undefined,
  keys: Record<string, (row: Row) => SortValue>,
): Row[] {
  const key = sorting && Object.hasOwn(keys, sorting.id) ? keys[sorting.id] : undefined;
  if (sorting === undefined || key === undefined) {
    return [...rows];
  }
  const direction = sorting.desc ? -1 : 1;
  return rows
    .map((row, index) => ({ row, index, value: key(row) }))
    .sort((left, right) => {
      if (left.value === undefined || right.value === undefined) {
        if (left.value === right.value) {
          return left.index - right.index;
        }
        return left.value === undefined ? 1 : -1;
      }
      const order =
        typeof left.value === "number" && typeof right.value === "number"
          ? left.value - right.value
          : String(left.value).localeCompare(String(right.value), undefined, TEXT_ORDER);
      return order !== 0 ? order * direction : left.index - right.index;
    })
    .map((entry) => entry.row);
}
