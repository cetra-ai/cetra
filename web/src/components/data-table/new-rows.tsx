import { type QueryKey, useQuery } from "@tanstack/react-query";
import type { ReactElement } from "react";
import { Button } from "@/components/ui/button";

/** "1 new span", "3 new spans", "50+ new spans": `more` adds "+" and forces the plural. */
export function countLabel(count: number, more: boolean, one: string, many: string): string {
  return `${count}${more ? "+" : ""} ${count === 1 && !more ? one : many}`;
}

/** The pill at the top of a list's table. */
export function NewRowsPill({
  label,
  onClick,
}: {
  label: string;
  onClick: () => void;
}): ReactElement {
  return (
    <Button type="button" size="xs" variant="secondary" onClick={onClick}>
      {label} ↑
    </Button>
  );
}

type CheckPage = { data: readonly unknown[]; next_cursor?: string | null };

/**
 * Asks once what arrived in `[anchor, now)`: the list's first page over that window, at most
 * NEW_ROWS_LIMIT rows. `options` is the list's options built with that window and the limit.
 * Returns undefined until the answer arrives, while disabled, or on an error.
 */
export function useNewRows(
  options: {
    queryKey: QueryKey;
    queryFn: (context: { pageParam: string | null; signal?: AbortSignal }) => Promise<CheckPage>;
  },
  enabled: boolean,
): { count: number; more: boolean } | undefined {
  const query = useQuery({
    queryKey: [...options.queryKey, "new-rows"],
    queryFn: ({ signal }) => options.queryFn({ pageParam: null, signal }),
    enabled,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: 0,
    retry: false,
  });
  if (query.data === undefined) {
    return undefined;
  }
  return {
    count: query.data.data.length,
    more: (query.data.next_cursor ?? null) !== null,
  };
}
