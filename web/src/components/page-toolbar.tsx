import type { ReactNode } from "react";

/**
 * A list page's toolbar (D-C-25): the search box, filters and summary on the left, in that order,
 * and the page's actions on the right. It has no title: the top bar names the page.
 */
export function PageToolbar({ children, actions }: { children?: ReactNode; actions?: ReactNode }) {
  return (
    <div
      data-slot="page-toolbar"
      className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b px-3"
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

/** The class of an item page's name in its toolbar (D-C-26). */
export const ITEM_TITLE_CLASS = "min-w-0 truncate text-ui font-medium";

/**
 * An item page's title in its toolbar (D-C-26): the back link, a slash, then the item's name,
 * given as an `h2` with `ITEM_TITLE_CLASS`.
 */
export function PageToolbarTitle({
  back,
  label,
  children,
}: {
  back: ReactNode;
  /** The nav's accessible name, such as "Evaluator". Not "Breadcrumb": the top bar's nav has that name. */
  label: string;
  children: ReactNode;
}) {
  return (
    <nav aria-label={label} className="flex min-w-0 shrink items-center gap-1.5">
      {back}
      <span aria-hidden="true" className="text-muted-foreground/60">
        /
      </span>
      {children}
    </nav>
  );
}
