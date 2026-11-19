import { Button } from "@/components/ui/button";
import type { ListKind } from "@/lib/facets";

const LABELS: Record<ListKind, string> = {
  traces: "Traces",
  spans: "Spans",
  sessions: "Sessions",
  users: "Users",
};

/**
 * Row 2's notice after a list was entered with filters from another list (D8): what was kept, and
 * how many filters were not.
 */
export function CarriedNotice({
  from,
  dropped,
  kept,
  onClear,
}: {
  from: ListKind;
  dropped: number;
  kept: boolean;
  onClear: () => void;
}) {
  const label = LABELS[from];
  const text = kept
    ? `Filters kept from ${label}`
    : `${dropped} ${dropped === 1 ? "filter" : "filters"} from ${label} not kept`;
  return (
    <div
      role="status"
      className="flex flex-wrap items-center gap-2 border-b px-3 py-1 text-caption text-muted-foreground"
    >
      <span>{text}</span>
      {kept && dropped > 0 ? <span>{`(${dropped} not kept)`}</span> : null}
      <Button type="button" variant="ghost" size="xs" onClick={onClear}>
        Clear
      </Button>
    </div>
  );
}
