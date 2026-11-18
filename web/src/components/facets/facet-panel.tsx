import { ListFilter } from "lucide-react";
import { useState } from "react";
import { FacetSection } from "@/components/facets/facet-section";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { splitConjuncts } from "@/lib/dsl/dsl";
import { FACETS, type FacetDef, type FacetView } from "@/lib/facets";

/** The window and flags a facet's counts are taken over: the list's own (D1, D3). */
export type FacetScope = {
  from: string;
  to: string;
  includeEval: boolean;
  includeExperiments: boolean;
  search?: string;
};

/** The props every section body takes. `filter` is the list's whole filter text. */
export type FacetBodyProps = {
  view: FacetView;
  def: FacetDef;
  filter: string;
  onFilter: (text: string) => void;
  scope: FacetScope;
};

const OPEN_KEY = "cetra.facets.open";

function expandedKey(view: FacetView): string {
  return `cetra.facets.${view}.expanded`;
}

function readStored(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    // Blocked storage: the panel keeps its defaults for this page view.
    return null;
  }
}

function writeStored(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Blocked storage: the choice lasts until the page is left.
  }
}

/** Whether the facet panel is open. Defaults to open; kept per browser (D3). */
export function useFacetsOpen(): [boolean, (open: boolean) => void] {
  const [open, setOpenState] = useState(() => readStored(OPEN_KEY) !== "0");
  function setOpen(next: boolean) {
    setOpenState(next);
    writeStored(OPEN_KEY, next ? "1" : "0");
  }
  return [open, setOpen];
}

/** The expanded section ids of a view: the stored choice, else the config's `defaultOpen` ones. */
function readExpanded(view: FacetView): string[] {
  const defaults = FACETS[view].filter((def) => def.defaultOpen === true).map((def) => def.id);
  const stored = readStored(expandedKey(view));
  if (stored === null) {
    return defaults;
  }
  try {
    const parsed: unknown = JSON.parse(stored);
    return Array.isArray(parsed) && parsed.every((item) => typeof item === "string")
      ? parsed
      : defaults;
  } catch {
    return defaults;
  }
}

/** The top-level conjunct count. A filter that cannot be split counts as one (never throws). */
function conjunctCount(filter: string): number {
  try {
    return splitConjuncts(filter).length;
  } catch {
    return filter.trim().length > 0 ? 1 : 0;
  }
}

/** The "Filters" button of row 2. Its badge counts the top-level conjuncts of the filter. */
export function FiltersToggle({
  open,
  onOpenChange,
  filter,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  filter: string;
}) {
  const count = conjunctCount(filter);
  return (
    <Button
      type="button"
      variant="outline"
      size="xs"
      aria-pressed={open}
      onClick={() => onOpenChange(!open)}
    >
      <ListFilter />
      Filters
      {count > 0 ? (
        <>
          {" "}
          <span className="rounded-sm bg-muted px-1 tabular-nums">{count}</span>{" "}
          <span className="sr-only">active</span>
        </>
      ) : null}
    </Button>
  );
}

/** The facet sidebar beside a list (D3). */
export function FacetPanel({
  view,
  filter,
  onFilter,
  scope,
  note,
}: {
  view: FacetView;
  filter: string;
  onFilter: (text: string) => void;
  scope: FacetScope;
  /** A muted line at the top, e.g. "Counts are traces" on the users list. */
  note?: string;
}) {
  const [search, setSearch] = useState("");
  const [expanded, setExpanded] = useState(() => readExpanded(view));
  const needle = search.trim().toLowerCase();
  const defs = FACETS[view].filter((def) => def.label.toLowerCase().includes(needle));

  function toggle(id: string) {
    const next = expanded.includes(id) ? expanded.filter((item) => item !== id) : [...expanded, id];
    setExpanded(next);
    writeStored(expandedKey(view), JSON.stringify(next));
  }

  return (
    <aside
      aria-label="Filter facets"
      className="flex min-h-0 w-60 shrink-0 flex-col gap-1 overflow-y-auto border-r p-2"
    >
      {note ? <p className="text-caption text-muted-foreground">{note}</p> : null}
      <Input
        type="search"
        aria-label="Search filters"
        placeholder="Search filters"
        value={search}
        onChange={(event) => setSearch(event.target.value)}
        className="h-7 shrink-0 text-caption"
      />
      {defs.length === 0 ? (
        <p className="px-1 py-2 text-caption text-muted-foreground">No matching filters</p>
      ) : null}
      {defs.map((def) => (
        <FacetSection
          key={def.id}
          view={view}
          def={def}
          filter={filter}
          onFilter={onFilter}
          scope={scope}
          expanded={expanded.includes(def.id)}
          onToggle={() => toggle(def.id)}
        />
      ))}
    </aside>
  );
}
