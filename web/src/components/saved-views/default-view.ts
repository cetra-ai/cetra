import { useQuery } from "@tanstack/react-query";
import { useLocation } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { meQuery, savedViewsQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import type { ListKind } from "@/lib/facets";
import { carriedSearch, readListContext } from "@/lib/list-context";
import { searchFromViewState } from "@/lib/search/common";

declare module "@tanstack/react-router" {
  interface HistoryState {
    /** Set by navigation that enters a list page (sidebar, palette, g shortcuts, project switcher). */
    applyDefaultView?: true;
  }
}

type SavedView = Schema<"SavedView">;

export function defaultSavedView(
  views: readonly SavedView[],
  kind: string,
  userId: string | undefined,
): SavedView | undefined {
  const defaults = views.filter((view) => view.kind === kind && view.is_default);
  const own = defaults.find((view) => userId !== undefined && view.owner_user_id === userId);
  if (own) {
    return own;
  }
  return defaults.find((view) => view.shared);
}

/** The filters carried into a list page, shown in its notice until the search changes (D8). */
export type CarriedList = { from: ListKind; dropped: number; kept: boolean };

/** A carry notice: `entry` is the search the page was entered with, `landed` the carried one. */
type Notice = { from: ListKind; dropped: number; kept: boolean; entry: string; landed?: string };

/**
 * Applies the caller's default view when the list page is entered with no search params (D-C-2).
 * With no default view matching, the list context of another list page of the project is carried
 * in instead (D8). `hold` keeps the list's query off until the entry search has been applied.
 */
export function useDefaultSavedView(
  kind: Schema<"SavedViewKind">,
  search: object,
  onApply: (search: Record<string, unknown>) => void,
  listKind: ListKind,
): { hold: boolean; carried: CarriedList | undefined } {
  const { org, project } = useProject();
  const location = useLocation();
  const entryKey = location.state.__TSR_key;
  const mountKey = useRef(entryKey);
  const appliedKey = useRef<string | undefined>(undefined);
  const [notice, setNotice] = useState<Notice | undefined>(undefined);
  const views = useQuery(savedViewsQuery(org, project, kind));
  const me = useQuery(meQuery);
  const searchKey = JSON.stringify(search);
  const empty = Object.keys(search).length === 0;
  // A return from a detail page restores the list's own search, so the default view does not apply.
  const restoring = location.state.listRestore !== undefined;
  const entered =
    !restoring && (entryKey === mountKey.current || location.state.applyDefaultView === true);
  const pending = empty && entered && appliedKey.current !== entryKey;
  const settled = views.isFetched && me.isFetched;
  const match =
    pending && settled
      ? defaultSavedView(views.data?.data ?? [], kind, me.data?.user.id)
      : undefined;
  // Read here, before anything is written: the list context is still the other page's.
  const carry =
    pending && settled && match === undefined
      ? carriedSearch(readListContext(org, project), listKind)
      : undefined;

  useEffect(() => {
    if (!pending || !settled) {
      return;
    }
    appliedKey.current = entryKey;
    if (match) {
      onApply({ ...searchFromViewState(match.state), view: match.id });
      return;
    }
    if (!carry) {
      return;
    }
    // Only drops: the carried search is the entry search, so there is nothing to navigate to. The
    // notice lands on the entry search at once, which releases hold.
    const navigates = Object.keys(carry.search).length > 0;
    setNotice({
      from: carry.from,
      dropped: carry.dropped,
      kept: carry.kept,
      entry: searchKey,
      ...(navigates ? {} : { landed: searchKey }),
    });
    if (navigates) {
      onApply(carry.search);
    }
  }, [pending, settled, match, carry, entryKey, onApply, searchKey]);

  // The notice follows the carried search: it lands once the search changes from the entry one,
  // and disappears on the next change after that.
  useEffect(() => {
    if (notice === undefined) {
      return;
    }
    if (notice.landed === undefined) {
      if (searchKey !== notice.entry) {
        setNotice({ ...notice, landed: searchKey });
      }
      return;
    }
    if (notice.landed !== searchKey) {
      setNotice(undefined);
    }
  }, [notice, searchKey]);

  const awaiting = notice !== undefined && notice.landed === undefined;
  const visible =
    notice !== undefined &&
    (notice.kept || notice.dropped > 0) &&
    (notice.landed === undefined ? searchKey !== notice.entry : notice.landed === searchKey);
  return {
    hold: (pending && (!settled || match !== undefined || carry !== undefined)) || awaiting,
    carried:
      visible && notice
        ? { from: notice.from, dropped: notice.dropped, kept: notice.kept }
        : undefined,
  };
}
