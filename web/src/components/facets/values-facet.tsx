import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import { Minus } from "lucide-react";
import { type ReactNode, useEffect, useId, useState } from "react";
import { facetQuery } from "@/api/queries";
import type { FacetBodyProps } from "@/components/facets/facet-panel";
import { TextFacet } from "@/components/facets/text-facet";
import { useProject } from "@/components/shell/project-context";
import { useRowHeight } from "@/components/theme-provider";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  fieldText,
  readText,
  readValues,
  type Selection,
  type SelectKind,
  withoutField,
  writeValues,
} from "@/lib/facets";
import { formatTokens } from "@/lib/format";

const SHOWN_FIRST = 8;
const FIRST_LIMIT = 10;
const WIDE_LIMIT = 50;
const SEARCH_DEBOUNCE_MS = 250;

function selectKindOf(kind: FacetBodyProps["def"]["kind"]): SelectKind {
  if (kind === "list") {
    return "list";
  }
  return kind === "bool" ? "bool" : "values";
}

/** A values facet (D3): a Select tab (checkboxes) and, for scalar fields, a Text tab. */
export function ValuesFacet(props: FacetBodyProps) {
  const { def, filter } = props;
  const canText = def.kind === "values";
  const [tab, setTab] = useState<"select" | "text">(() =>
    canText && readText(filter, def.field, "values") !== undefined ? "text" : "select",
  );
  return (
    <div className="flex flex-col gap-1.5">
      {canText ? (
        <Tabs value={tab} onValueChange={(value) => setTab(value === "text" ? "text" : "select")}>
          <TabsList className="h-7 w-full">
            <TabsTrigger value="select">Select</TabsTrigger>
            <TabsTrigger value="text">Text</TabsTrigger>
          </TabsList>
        </Tabs>
      ) : null}
      {canText && tab === "text" ? (
        <TextFacet {...props} />
      ) : (
        <SelectValues {...props} selectKind={selectKindOf(def.kind)} />
      )}
    </div>
  );
}

type Row = { value: string; count?: number };

/** The Select tab: checkboxes of the field's values with counts, "(none)" and Exclude (D3, D4). */
function SelectValues({
  view,
  def,
  filter,
  onFilter,
  scope,
  selectKind,
}: FacetBodyProps & { selectKind: SelectKind }) {
  const { org, project } = useProject();
  const rowHeight = useRowHeight();
  const [draft, setDraft] = useState("");
  const [q, setQ] = useState("");
  const [showMore, setShowMore] = useState(false);
  const [wide, setWide] = useState(false);
  const idBase = useId();
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
      limit: wide ? WIDE_LIMIT : FIRST_LIMIT,
    }),
  );
  const data = query.data;
  const all = data?.values ?? [];
  const shown = showMore ? all : all.slice(0, SHOWN_FIRST);
  const canShowMore = !showMore && (all.length > SHOWN_FIRST || data?.more === true);
  const { selection, mixed } = readValues(filter, def.field, selectKind);
  const isList = def.kind === "list";

  // Selected values come first, even when the cut or the search hides them from the answer.
  const selectedRows: Row[] = selection.values.map((value) => ({
    value,
    count: all.find((item) => item.value === value)?.count,
  }));
  const otherRows: Row[] = shown
    .filter((item) => !selection.values.includes(item.value))
    .map((item) => ({ value: item.value, count: item.count }));
  const rows = [...selectedRows, ...otherRows];

  function write(next: Selection) {
    onFilter(writeValues(filter, def.field, selectKind, next));
  }

  function setValue(value: string, checked: boolean) {
    const fresh = selection.values.length === 0 && !selection.none;
    const others = selection.values.filter((item) => item !== value);
    write({
      mode: fresh ? "include" : selection.mode,
      values: checked ? [...others, value] : others,
      none: false,
    });
  }

  function setNone(checked: boolean) {
    write({ mode: selection.mode, values: [], none: checked });
  }

  function exclude(value: string) {
    const values =
      selection.mode === "exclude"
        ? [...selection.values.filter((item) => item !== value), value]
        : [value];
    write({ mode: "exclude", values, none: false });
  }

  let body: ReactNode;
  if (query.isError) {
    body = <p className="text-caption text-muted-foreground">Could not load values</p>;
  } else if (!data) {
    body = (
      <div className="flex flex-col gap-1.5">
        <Skeleton className="h-4 w-full" />
        <Skeleton className="h-4 w-full" />
        <Skeleton className="h-4 w-full" />
      </div>
    );
  } else {
    body = (
      <>
        {rows.map((row, index) => {
          const id = `${idBase}-${index}`;
          const isSelected = selection.values.includes(row.value);
          const excluded = isSelected && selection.mode === "exclude";
          return (
            <label
              key={row.value}
              htmlFor={id}
              className="group flex shrink-0 items-center gap-2"
              style={{ height: rowHeight }}
            >
              <Checkbox
                id={id}
                aria-label={excluded ? `Excluded ${row.value}` : row.value}
                checked={isSelected}
                disabled={mixed}
                onCheckedChange={(checked) => setValue(row.value, checked === true)}
              />
              <span
                title={row.value}
                className={cn("min-w-0 flex-1 truncate text-body", excluded && "line-through")}
              >
                {row.value}
              </span>
              <span className="shrink-0 text-caption tabular-nums text-muted-foreground">
                {row.count === undefined ? "–" : formatTokens(row.count)}
              </span>
              {!isList && !mixed ? (
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  aria-label={`Exclude ${row.value}`}
                  className="shrink-0 opacity-0 group-hover:opacity-100 focus-visible:opacity-100"
                  onClick={() => exclude(row.value)}
                >
                  <Minus />
                </Button>
              ) : null}
            </label>
          );
        })}
        {data.missing > 0 && !isList ? (
          <label
            htmlFor={`${idBase}-none`}
            className="flex shrink-0 items-center gap-2"
            style={{ height: rowHeight }}
          >
            <Checkbox
              id={`${idBase}-none`}
              aria-label="(none)"
              checked={selection.none}
              disabled={mixed}
              onCheckedChange={(checked) => setNone(checked === true)}
            />
            <span className="min-w-0 flex-1 truncate text-body text-muted-foreground">(none)</span>
            <span className="shrink-0 text-caption tabular-nums text-muted-foreground">
              {formatTokens(data.missing)}
            </span>
          </label>
        ) : null}
        {all.length === 0 && data.missing === 0 && selection.values.length === 0 ? (
          <p className="text-caption text-muted-foreground">No values</p>
        ) : null}
        {canShowMore ? (
          <Button
            type="button"
            variant="ghost"
            size="xs"
            className="shrink-0 self-start"
            onClick={() => {
              setShowMore(true);
              if (data.more) {
                setWide(true);
              }
            }}
          >
            Show more
          </Button>
        ) : null}
        {showMore && data.more ? (
          <p className="text-caption text-muted-foreground">Refine the search to see more</p>
        ) : null}
      </>
    );
  }

  return (
    <div className="flex flex-col gap-1.5">
      <Input
        type="search"
        aria-label={`Search ${def.label} values`}
        placeholder="Search values"
        value={draft}
        maxLength={200}
        onChange={(event) => setDraft(event.target.value)}
        className="h-7 shrink-0 text-caption"
      />
      {mixed ? <p className="text-caption text-muted-foreground">Edited as text</p> : null}
      {body}
    </div>
  );
}
