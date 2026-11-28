import { MessagesSquare, Rows3 } from "lucide-react";
import type { ComponentType } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Toggle } from "@/components/ui/toggle";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  setTurnDensity,
  TURN_FILTERS,
  type TurnDensity,
  type TurnFilter,
  useTurnDensity,
} from "@/lib/session-turns";

const DENSITY_OPTIONS: readonly {
  id: TurnDensity;
  label: string;
  icon: ComponentType;
}[] = [
  { id: "chat", label: "Chat", icon: MessagesSquare },
  { id: "compact", label: "Compact", icon: Rows3 },
];

/**
 * The search, the filter chips and the density switch above a session's turns (D-C-28). The
 * "Uses tools" chip is hidden until the steps have loaded.
 */
export function TurnToolbar({
  query,
  onQuery,
  filters,
  onToggleFilter,
  counts,
  stepsReady,
}: {
  query: string;
  onQuery: (next: string) => void;
  filters: ReadonlySet<TurnFilter>;
  onToggleFilter: (filter: TurnFilter) => void;
  counts: Record<TurnFilter, number>;
  /** False until the steps are ready: the "Uses tools" chip is hidden. */
  stepsReady: boolean;
}) {
  return (
    <div className="flex flex-wrap items-center gap-2 border-b px-4 py-2">
      <Input
        aria-label="Search turns"
        placeholder="Search turns"
        className="h-7 w-56 text-body"
        value={query}
        onChange={(event) => {
          onQuery(event.target.value);
        }}
      />
      {TURN_FILTERS.map(({ id, label }) => {
        if (id === "tools" && !stepsReady) {
          return null;
        }
        return (
          <Toggle
            key={id}
            size="sm"
            className="h-6 px-2"
            pressed={filters.has(id)}
            onPressedChange={() => {
              onToggleFilter(id);
            }}
          >
            {label} <span className="num">{counts[id]}</span>
          </Toggle>
        );
      })}
      <div className="ml-auto">
        <DensitySwitch />
      </div>
    </div>
  );
}

/** Chat or Compact, remembered per browser (`cetra.session_density`). */
function DensitySwitch() {
  const density = useTurnDensity();
  return (
    <div
      role="radiogroup"
      aria-label="Density"
      className="inline-flex shrink-0 items-center rounded-md border"
    >
      {DENSITY_OPTIONS.map(({ id, label, icon: Icon }) => (
        <Tooltip key={id}>
          <TooltipTrigger asChild>
            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              role="radio"
              aria-checked={density === id}
              aria-label={label}
              className={density === id ? "bg-muted" : undefined}
              onClick={() => {
                setTurnDensity(id);
              }}
            >
              <Icon />
            </Button>
          </TooltipTrigger>
          <TooltipContent>{label}</TooltipContent>
        </Tooltip>
      ))}
    </div>
  );
}
