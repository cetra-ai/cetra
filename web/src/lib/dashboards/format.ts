import { durationTicks, niceTicks, percentTicks } from "@/lib/chart-scale";
import { formatCost, formatDuration } from "@/lib/format";
import { formatCount, formatPercent } from "@/lib/metrics";
import { fieldKind } from "./fields";
import type { Measure } from "./types";

/** How a measure's values are formatted: by what the field counts or measures. */
export type MeasureKind = "count" | "rate" | "duration" | "cost" | "score" | "number";

/** Kind of a measure: counts and rates by aggregate, anything else by its field. */
export function measureKind(measure: Measure): MeasureKind {
  switch (measure.agg) {
    case "count":
    case "count_distinct":
      return "count";
    case "rate":
      return "rate";
    default:
      return measure.field ? fieldKind(measure.field) : "number";
  }
}

/** The legend name of a measure: its label, else its aggregate and field. */
export function measureLabel(measure: Measure): string {
  if (measure.label) {
    return measure.label;
  }
  if (measure.agg === "count" || measure.agg === "rate" || !measure.field) {
    return measure.agg;
  }
  return `${measure.agg} of ${measure.field}`;
}

const DECIMALS = new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 });

/** A value of a kind, or `—` for a gap. */
export function formatValue(kind: MeasureKind, value: number | null | undefined): string {
  if (value === null || value === undefined || Number.isNaN(value)) {
    return "—";
  }
  switch (kind) {
    case "count":
      return formatCount(value);
    case "rate":
      return formatPercent(value);
    case "duration":
      return formatDuration(value);
    case "cost":
      return formatCost(value);
    case "score":
    case "number":
      return DECIMALS.format(value);
  }
}

/** Round ticks from 0 to the top of `max`, with the labels of the kind. */
export function axisTicks(
  kind: MeasureKind,
  max: number,
): { ticks: number[]; format: (value: number) => string } {
  switch (kind) {
    case "duration":
      return durationTicks(max);
    case "rate":
      return { ticks: percentTicks(max), format: formatPercent };
    case "cost":
      return { ticks: niceTicks(max, 4, 0), format: formatCost };
    case "count":
      return { ticks: niceTicks(max, 4, 1), format: formatCount };
    case "score":
      return { ticks: niceTicks(max, 4, 0.01), format: (value) => DECIMALS.format(value) };
    case "number":
      return { ticks: niceTicks(max, 4, 0), format: (value) => DECIMALS.format(value) };
  }
}
