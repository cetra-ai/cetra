import type { ReactNode } from "react";

/**
 * A list page's toolbar (D-C-25): the search box, filters and summary on the left, in that order,
 * and the page's actions on the right. It has no title: the top bar names the page.
 */
export function PageToolbar({ children, actions }: { children?: ReactNode; actions?: ReactNode }) {
  return (
    <div
      data-slot="page-toolbar"
      className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b px-3 py-1"
    >
      {children}
      {actions === undefined || actions === null ? null : (
        <div className="ml-auto flex shrink-0 items-center gap-2">{actions}</div>
      )}
    </div>
  );
}

/** Muted totals in a page toolbar, after the filters. */
export function PageToolbarSummary({ children }: { children: ReactNode }) {
  return (
    <div className="flex shrink-0 items-center gap-2 text-body tabular-nums text-muted-foreground">
      {children}
    </div>
  );
}
