export type ChartRow = { time: number } & Record<string, number | null>;

export type KpiDirection = "up" | "down" | "flat";

export type KpiDelta = {
  value?: number;
  previous?: number;
  /** Undefined when the previous value is 0. */
  changePct?: number;
  /** Set when the previous value is 0 and the value is above 0. */
  isNew?: true;
  direction: KpiDirection;
};

/** The five series colours of the charts; a group takes the one at its colour index. */
export const PALETTE = [
  "var(--chart-1)",
  "var(--chart-2)",
  "var(--chart-3)",
  "var(--chart-4)",
  "var(--chart-5)",
] as const;

const NS_PER_MS = 1_000_000;

/**
 * A colour index for each group. The ranked groups (the models by cost, in that order) keep their
 * place in the ranking, whether or not a chart shows them; the other groups follow alphabetically.
 */
export function groupColorIndex(
  ranked: readonly string[],
  present: Iterable<string>,
): Map<string, number> {
  const index = new Map<string, number>();
  for (const group of ranked) {
    if (!index.has(group)) {
      index.set(group, index.size);
    }
  }
  const rest = [...new Set(present)]
    .filter((group) => !index.has(group))
    .sort((left, right) => left.localeCompare(right));
  for (const group of rest) {
    index.set(group, index.size);
  }
  return index;
}

/** Delta of a KPI against the previous window of the same length. */
export function kpiDelta(kpi: { value?: number; previous?: number }): KpiDelta {
  const { value, previous } = kpi;
  if (value === undefined || previous === undefined) {
    return { value, previous, direction: "flat" };
  }
  const direction: KpiDirection = value > previous ? "up" : value < previous ? "down" : "flat";
  if (previous === 0) {
    return value > 0 ? { value, previous, isNew: true, direction } : { value, previous, direction };
  }
  const changePct = ((value - previous) / previous) * 100;
  return { value, previous, changePct, direction };
}

/** `[from, to)` for the bucket that starts at `timeMs`. Both ends are RFC 3339 UTC. */
export function bucketRange(timeMs: number, bucketNs: number): { from: string; to: string } {
  return {
    from: new Date(timeMs).toISOString(),
    to: new Date(timeMs + bucketNs / NS_PER_MS).toISOString(),
  };
}

export function formatPercent(rate: number): string {
  const percent = rate * 100;
  const rounded = Math.round(percent * 10) / 10;
  return Number.isInteger(rounded) ? `${rounded}%` : `${rounded.toFixed(1)}%`;
}

export function formatCount(value: number): string {
  return new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(Math.round(value));
}

/** The time of an RFC 3339 instant in epoch milliseconds, or `undefined` when it does not parse. */
export function epochMs(rfc3339: string): number | undefined {
  const parsed = Date.parse(rfc3339);
  if (!Number.isNaN(parsed)) {
    return parsed;
  }
  const trimmed = Date.parse(rfc3339.replace(/\.(\d{3})\d+/, ".$1"));
  return Number.isNaN(trimmed) ? undefined : trimmed;
}
