export const numbers = new Intl.NumberFormat("en-US");

/** The date part of an RFC 3339 timestamp. */
export function day(iso: string): string {
  return iso.slice(0, 10);
}

/**
 * Percent of the base the month's units use: `included`, or `cap` when no included value is set.
 * Undefined when there is no base, or it is 0. Not capped at 100.
 */
export function usagePercent(units: number, included?: number, cap?: number): number | undefined {
  const base = included ?? cap;
  if (base === undefined || base === 0) {
    return undefined;
  }
  return (units / base) * 100;
}

function barClass(percent: number | undefined): string {
  if (percent !== undefined && percent >= 100) {
    return "bg-status-error";
  }
  if (percent !== undefined && percent >= 80) {
    return "bg-status-warn";
  }
  return "bg-primary";
}

/**
 * The month's units against `units_included`, or `units_cap` when no included value is set.
 * With neither, only the text shows. `compact` drops the reset and cap lines, for the sidebar.
 */
export function UsageMeter({
  units,
  resetsAt,
  included,
  cap,
  variant = "full",
}: {
  units: number;
  resetsAt: string;
  included: number | undefined;
  cap: number | undefined;
  variant?: "full" | "compact";
}) {
  const base = included ?? cap;
  const percent = usagePercent(units, included, cap);
  const compact = variant === "compact";
  const text =
    base === undefined
      ? `${numbers.format(units)} units`
      : compact
        ? `${numbers.format(units)} of ${numbers.format(base)} units · ${Math.round(percent ?? 0)}%`
        : `${numbers.format(units)} of ${numbers.format(base)} units`;

  return (
    <div className="flex flex-col gap-2">
      {base === undefined ? null : (
        // biome-ignore lint/a11y/useSemanticElements: a native meter cannot show the 80 % and 100 % marks, so the styled bar stays a div with role meter
        <div
          role="meter"
          aria-label="Units this month"
          aria-valuemin={0}
          aria-valuemax={base}
          aria-valuenow={Math.min(units, base)}
          className={`relative w-full overflow-hidden rounded-full bg-muted ${compact ? "h-1.5" : "h-3"}`}
        >
          <div
            className={`h-full ${barClass(percent)}`}
            style={{ width: `${Math.min(100, percent ?? 0)}%` }}
          />
          {[80, 100].map((mark) => (
            <span
              key={mark}
              aria-hidden="true"
              className="absolute top-0 h-full w-px bg-foreground/50"
              style={{ left: `${mark}%` }}
            />
          ))}
        </div>
      )}
      <p className={compact ? "text-caption tabular-nums" : "text-body tabular-nums"}>{text}</p>
      {compact ? null : (
        <p className="text-body text-muted-foreground">Resets on {day(resetsAt)} (UTC).</p>
      )}
      {compact || cap === undefined ? null : (
        <p className="text-body text-muted-foreground">
          Ingest pauses at {numbers.format(cap)} units.
        </p>
      )}
    </div>
  );
}
