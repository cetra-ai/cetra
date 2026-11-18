import { type FormEvent, type ReactNode, useState } from "react";
import { useFacetQuery } from "@/api/queries";
import type { FacetBodyProps } from "@/components/facets/facet-panel";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { type FacetDef, fieldText, readRange, withoutField, writeRange } from "@/lib/facets";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";

type Histogram = {
  min: number;
  max: number;
  p50: number;
  p95: number;
  p99: number;
  buckets: { lo: number; hi: number; count: number }[];
};

/** Formats a number of the field's unit (D5). Histogram values are in the column's own unit. */
function formatterOf(unit: FacetDef["unit"]): (value: number) => string {
  switch (unit) {
    case "duration_ms":
      // formatDuration takes nanoseconds; the column is in milliseconds.
      return (value) => formatDuration(value * 1_000_000);
    case "usd":
      return (value) => formatCost(value);
    default:
      return (value) => formatTokens(value);
  }
}

/** The unit shown after a bound input: ms, $, or nothing for counts (D5). */
function suffixOf(unit: FacetDef["unit"]): string {
  switch (unit) {
    case "duration_ms":
      return "ms";
    case "usd":
      return "$";
    default:
      return "";
  }
}

/** Whether a bar is inside the selected range: it overlaps `[lo, hi)`. No range selects nothing. */
function overlaps(bar: { lo: number; hi: number }, lo?: number, hi?: number): boolean {
  if (lo === undefined && hi === undefined) {
    return false;
  }
  return (lo === undefined || bar.hi > lo) && (hi === undefined || bar.lo < hi);
}

/** The smallest non-zero magnitude a bound may have: smaller ones are not written (D5). */
const MIN_BOUND = 1e-9;

/**
 * A bound input as a number: `undefined` when empty (unbounded), `NaN` when it cannot be written:
 * not a finite number, or a non-zero magnitude below MIN_BOUND.
 */
function parseBound(text: string): number | undefined {
  if (text.trim() === "") {
    return undefined;
  }
  const value = Number(text);
  const writable = Number.isFinite(value) && (value === 0 || Math.abs(value) >= MIN_BOUND);
  return writable ? value : Number.NaN;
}

/** Whether a typed bound is non-zero but too small to write (its Apply is disabled). */
function tooSmall(text: string): boolean {
  const value = Number(text);
  return text.trim() !== "" && Number.isFinite(value) && value !== 0 && Math.abs(value) < MIN_BOUND;
}

function textOf(bound: number | undefined): string {
  return bound === undefined ? "" : String(bound);
}

/** A range facet of a number field (D5): histogram bars, percentiles and min/max inputs. */
export function RangeFacet({ view, def, filter, onFilter, scope }: FacetBodyProps) {
  const { org, project } = useProject();
  const query = useFacetQuery(org, project, {
    view,
    field: fieldText(def.field),
    from: scope.from,
    to: scope.to,
    includeEval: scope.includeEval,
    includeExperiments: scope.includeExperiments,
    search: scope.search,
    filter: withoutField(filter, def.field) || undefined,
  });
  const { lo, hi, mixed } = readRange(filter, def.field);
  const format = formatterOf(def.unit);
  const histogram: Histogram | undefined = query.data?.histogram;

  let body: ReactNode;
  if (query.isError) {
    body = <p className="text-caption text-muted-foreground">Could not load values</p>;
  } else if (!query.data) {
    body = <Skeleton className="h-12 w-full" />;
  } else if (!histogram) {
    body = <p className="text-caption text-muted-foreground">No values</p>;
  } else {
    body = (
      <Bars
        histogram={histogram}
        format={format}
        lo={lo}
        hi={hi}
        disabled={mixed}
        onSelect={(nextLo, nextHi) => onFilter(writeRange(filter, def.field, nextLo, nextHi))}
      />
    );
  }

  return (
    <div className="flex flex-col gap-1.5">
      {mixed ? <p className="text-caption text-muted-foreground">Edited as text</p> : null}
      {body}
      {histogram ? (
        <RangeInputs
          // Remounted when the filter's range changes, so the inputs show the filter's bounds.
          key={`${textOf(lo)}:${textOf(hi)}:${String(mixed)}`}
          label={def.label}
          suffix={suffixOf(def.unit)}
          disabled={mixed}
          onApply={(nextLo, nextHi) => onFilter(writeRange(filter, def.field, nextLo, nextHi))}
          lo={lo}
          hi={hi}
        />
      ) : null}
    </div>
  );
}

function Bars({
  histogram,
  format,
  lo,
  hi,
  disabled,
  onSelect,
}: {
  histogram: Histogram;
  format: (value: number) => string;
  lo?: number;
  hi?: number;
  disabled: boolean;
  onSelect: (lo: number | undefined, hi: number | undefined) => void;
}) {
  const { buckets } = histogram;
  const top = Math.max(0, ...buckets.map((bucket) => bucket.count));
  const last = buckets.length - 1;
  return (
    <div className="flex flex-col gap-1">
      <div className="flex h-12 items-end gap-px">
        {buckets.map((bucket, index) => {
          const isLast = index === last;
          const selected = overlaps(bucket, lo, hi);
          const label = `${format(bucket.lo)}–${format(bucket.hi)}: ${bucket.count}`;
          const height = top === 0 ? 0 : Math.round((bucket.count / top) * 100);
          return (
            <button
              key={`${bucket.lo}:${bucket.hi}`}
              type="button"
              disabled={disabled}
              aria-label={label}
              title={label}
              className="focus-ring flex h-full min-w-0 flex-1 items-end rounded-sm disabled:opacity-50"
              onClick={(event) => {
                if (event.shiftKey && (lo !== undefined || hi !== undefined)) {
                  // An open upper bound closes at the clicked bar, unless that is the last bar.
                  onSelect(
                    lo === undefined ? undefined : Math.min(lo, bucket.lo),
                    isLast ? undefined : Math.max(hi ?? bucket.hi, bucket.hi),
                  );
                  return;
                }
                // The last bucket is closed at max, so it has no upper bound (D5).
                onSelect(bucket.lo, isLast ? undefined : bucket.hi);
              }}
            >
              <span
                className={`block w-full rounded-t-sm ${selected ? "bg-primary" : "bg-muted-foreground/40"}`}
                style={{ height: `${height}%` }}
              />
            </button>
          );
        })}
      </div>
      <p className="flex flex-wrap gap-x-2 text-caption text-muted-foreground tabular-nums">
        <span>p50 {format(histogram.p50)}</span>
        <span>p95 {format(histogram.p95)}</span>
        <span>p99 {format(histogram.p99)}</span>
      </p>
    </div>
  );
}

function RangeInputs({
  label,
  suffix,
  disabled,
  lo,
  hi,
  onApply,
}: {
  label: string;
  suffix: string;
  disabled: boolean;
  lo?: number;
  hi?: number;
  onApply: (lo: number | undefined, hi: number | undefined) => void;
}) {
  const [minText, setMinText] = useState(textOf(lo));
  const [maxText, setMaxText] = useState(textOf(hi));
  const min = parseBound(minText);
  const max = parseBound(maxText);
  const invalid =
    (min !== undefined && Number.isNaN(min)) || (max !== undefined && Number.isNaN(max));
  const crossed = min !== undefined && max !== undefined && !invalid && min >= max;
  const canApply = !disabled && !invalid && !crossed;
  const small = tooSmall(minText) || tooSmall(maxText);

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (canApply) {
      onApply(min, max);
    }
  }

  return (
    <form className="flex flex-col gap-1.5" onSubmit={submit}>
      <div className="flex min-w-0 items-center gap-1">
        <Input
          aria-label={`${label} min`}
          placeholder="min"
          inputMode="decimal"
          value={minText}
          disabled={disabled}
          onChange={(event) => setMinText(event.target.value)}
          className="h-7 min-w-0 flex-1 text-caption"
        />
        {suffix ? <span className="text-caption text-muted-foreground">{suffix}</span> : null}
        <Input
          aria-label={`${label} max`}
          placeholder="max"
          inputMode="decimal"
          value={maxText}
          disabled={disabled}
          onChange={(event) => setMaxText(event.target.value)}
          className="h-7 min-w-0 flex-1 text-caption"
        />
        {suffix ? <span className="text-caption text-muted-foreground">{suffix}</span> : null}
      </div>
      {small ? (
        <p className="text-caption text-muted-foreground">Too small; use 0 or a larger value</p>
      ) : null}
      {crossed ? <p className="text-caption text-muted-foreground">Min must be below max</p> : null}
      <Button type="submit" size="xs" className="self-start" disabled={!canApply}>
        Apply
      </Button>
    </form>
  );
}
