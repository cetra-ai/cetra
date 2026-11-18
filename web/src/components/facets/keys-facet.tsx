import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronRight } from "lucide-react";
import { useEffect, useState } from "react";
import { facetQuery } from "@/api/queries";
import type { FacetBodyProps } from "@/components/facets/facet-panel";
import { ValuesFacet } from "@/components/facets/values-facet";
import { useProject } from "@/components/shell/project-context";
import { useRowHeight } from "@/components/theme-provider";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { Field } from "@/lib/dsl/dsl";
import {
  clearField,
  type FacetDef,
  fieldText,
  keysInFilter,
  ownedConjuncts,
  withoutField,
} from "@/lib/facets";
import { formatTokens } from "@/lib/format";

/** How many keys a map column lists. Not fixed by the plan; chosen here. */
const KEYS_LIMIT = 20;
const SEARCH_DEBOUNCE_MS = 250;

/**
 * A map column's keys with counts (D3). Opening a key shows a values facet of `map['key']`. The
 * keys the filter already names start open, and an open key stays listed when the search hides it.
 */
export function KeysFacet({ view, def, filter, onFilter, scope }: FacetBodyProps) {
  const { org, project } = useProject();
  const rowHeight = useRowHeight();
  const column = def.field.column;
  const [open, setOpen] = useState<string[]>(() => keysInFilter(filter, column));
  const [draft, setDraft] = useState("");
  const [q, setQ] = useState("");
  useEffect(() => {
    const timer = window.setTimeout(() => setQ(draft.trim()), SEARCH_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [draft]);

  const query = useQuery(
    facetQuery(org, project, {
      view,
      field: fieldText(def.field),
      from: scope.from,
      to: scope.to,
      includeEval: scope.includeEval,
      includeExperiments: scope.includeExperiments,
      search: scope.search,
      filter: withoutField(filter, def.field) || undefined,
      q: q || undefined,
      limit: KEYS_LIMIT,
    }),
  );

  function toggle(key: string) {
    setOpen((current) =>
      current.includes(key) ? current.filter((item) => item !== key) : [...current, key],
    );
  }

  const searchBox = (
    <Input
      type="search"
      aria-label={`Search ${def.label} keys`}
      placeholder="Search keys"
      value={draft}
      maxLength={200}
      onChange={(event) => setDraft(event.target.value)}
      className="h-7 shrink-0 text-caption"
    />
  );

  if (query.isError) {
    return (
      <div className="flex flex-col gap-1.5">
        {searchBox}
        <p className="text-caption text-muted-foreground">Could not load keys</p>
      </div>
    );
  }
  if (!query.data) {
    return (
      <div className="flex flex-col gap-1.5">
        {searchBox}
        <p className="text-caption text-muted-foreground">Loading keys…</p>
      </div>
    );
  }
  const answered = query.data.values ?? [];
  // Open keys first (even when the search hides them), then the answer.
  const openOnly = open.filter((key) => !answered.some((item) => item.value === key));
  const rows: { value: string; count?: number }[] = [
    ...openOnly.map((value) => ({ value })),
    ...answered.map((item) => ({ value: item.value, count: item.count })),
  ];
  return (
    <div className="flex flex-col gap-1">
      {searchBox}
      {rows.length === 0 ? <p className="text-caption text-muted-foreground">No keys</p> : null}
      {rows.map((row) => {
        const isOpen = open.includes(row.value);
        // Keys are only offered for map columns (FACETS), so the column is a map column here.
        const field = { column, key: row.value } as Field;
        const nested: FacetDef = {
          id: `${def.id}.${row.value}`,
          field,
          label: row.value,
          kind: "values",
        };
        const owned = ownedConjuncts(filter, field, "values").length;
        const Chevron = isOpen ? ChevronDown : ChevronRight;
        return (
          <div key={row.value} className="flex shrink-0 flex-col">
            <button
              type="button"
              aria-expanded={isOpen}
              onClick={() => toggle(row.value)}
              className="focus-ring flex w-full items-center gap-2 rounded-sm text-left"
              style={{ height: rowHeight }}
            >
              <Chevron className="size-3.5 shrink-0 text-muted-foreground" />
              <span title={row.value} className="min-w-0 flex-1 truncate text-body">
                {row.value}
              </span>
              {owned > 0 ? (
                <span className="shrink-0 rounded-sm bg-primary/10 px-1 text-caption tabular-nums">
                  {owned}
                </span>
              ) : null}
              {row.count === undefined ? null : (
                <span className="shrink-0 text-caption tabular-nums text-muted-foreground">
                  {formatTokens(row.count)}
                </span>
              )}
            </button>
            {isOpen ? (
              <div className="ml-4 flex flex-col gap-1.5 border-l pl-2 pb-1">
                {owned > 0 ? (
                  <Button
                    type="button"
                    variant="ghost"
                    size="xs"
                    className="shrink-0 self-start"
                    onClick={() => onFilter(clearField(filter, field))}
                  >
                    Clear {row.value}
                  </Button>
                ) : null}
                <ValuesFacet
                  view={view}
                  def={nested}
                  filter={filter}
                  onFilter={onFilter}
                  scope={scope}
                />
              </div>
            ) : null}
          </div>
        );
      })}
      {query.data.more === true ? (
        <p className="text-caption text-muted-foreground">Refine the search to see more</p>
      ) : null}
    </div>
  );
}
