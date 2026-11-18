import { ChevronDown, ChevronRight } from "lucide-react";
import type { FacetBodyProps } from "@/components/facets/facet-panel";
import { KeysFacet } from "@/components/facets/keys-facet";
import { RangeFacet } from "@/components/facets/range-facet";
import { TextFacet } from "@/components/facets/text-facet";
import { ValuesFacet } from "@/components/facets/values-facet";
import { Button } from "@/components/ui/button";
import { clearField, clearKeys, ownedConjuncts, ownedKeyConjuncts } from "@/lib/facets";

/** One collapsible section of the facet panel: header with badge and Clear, body by kind (D3). */
export function FacetSection({
  expanded,
  onToggle,
  ...props
}: FacetBodyProps & { expanded: boolean; onToggle: () => void }) {
  const { def, filter, onFilter } = props;
  // A keys section owns the conjuncts of every key of its map column (D3).
  const isKeys = def.kind === "keys";
  const owned = isKeys
    ? ownedKeyConjuncts(filter, def.field.column).length
    : ownedConjuncts(filter, def.field, def.kind).length;
  const Chevron = expanded ? ChevronDown : ChevronRight;
  return (
    <section className="flex shrink-0 flex-col gap-1.5 border-b py-2">
      <div className="flex shrink-0 items-center gap-1">
        <button
          type="button"
          aria-expanded={expanded}
          onClick={onToggle}
          className="focus-ring flex min-w-0 flex-1 items-center gap-1 rounded-sm text-left text-ui font-medium"
        >
          <Chevron className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="truncate">{def.label}</span>
          {owned > 0 ? (
            <span className="shrink-0 rounded-sm bg-primary/10 px-1 text-caption font-normal tabular-nums">
              {owned}
            </span>
          ) : null}
        </button>
        {owned > 0 ? (
          <Button
            type="button"
            variant="ghost"
            size="xs"
            onClick={() =>
              onFilter(isKeys ? clearKeys(filter, def.field.column) : clearField(filter, def.field))
            }
          >
            Clear
          </Button>
        ) : null}
      </div>
      {expanded ? <FacetBody {...props} /> : null}
    </section>
  );
}

function FacetBody(props: FacetBodyProps) {
  switch (props.def.kind) {
    case "values":
    case "list":
    case "bool":
      return <ValuesFacet {...props} />;
    case "text":
      return <TextFacet {...props} />;
    case "keys":
      return <KeysFacet {...props} />;
    case "range":
      return <RangeFacet {...props} />;
  }
}
