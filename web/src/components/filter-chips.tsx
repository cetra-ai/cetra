import { cn } from "cn";

export type FilterChip<T extends string> = {
  value: T;
  label: string;
  /** Shown after the label when known. */
  count?: number;
  /** A Tailwind background class for a status dot before the label. */
  dot?: string;
};

/**
 * Optional filters shown as chips with counts (D-C-25). Selecting a chip filters on it; selecting
 * the selected chip clears the filter. No chip stands for "all".
 */
export function FilterChips<T extends string>({
  label,
  chips,
  active,
  onSelect,
}: {
  label: string;
  chips: readonly FilterChip<T>[];
  active: T | undefined;
  onSelect: (value: T | undefined) => void;
}) {
  return (
    <section aria-label={label} className="flex shrink-0 items-center gap-1.5 text-body">
      {chips.map((chip) => {
        const selected = active === chip.value;
        return (
          <button
            key={chip.value}
            type="button"
            aria-pressed={selected}
            onClick={() => onSelect(selected ? undefined : chip.value)}
            className={cn(
              "inline-flex h-6 items-center gap-1.5 rounded-md border px-2 hover:bg-muted",
              selected && "border-foreground/30 bg-muted font-medium",
            )}
          >
            {chip.dot === undefined ? null : (
              <span aria-hidden="true" className={cn("size-1.5 rounded-full", chip.dot)} />
            )}
            <span>{chip.label}</span>
            {chip.count === undefined ? null : (
              <>
                {" "}
                <span className="tabular-nums">{chip.count}</span>
              </>
            )}
          </button>
        );
      })}
    </section>
  );
}
