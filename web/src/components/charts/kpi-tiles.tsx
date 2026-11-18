import { cn } from "cn";
import { ArrowDown, ArrowUp, Minus } from "lucide-react";
import type { Schema } from "@/api/types";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  formatKpi,
  type KpiDelta,
  type KpiDirection,
  type KpiMetric,
  kpiDelta,
} from "@/lib/metrics";

/**
 * What a rise in a metric means. `up-good`: a rise is good news. `up-bad`: a
 * rise is bad news. `neutral`: the delta is shown in the muted colour.
 */
export type KpiTone = "up-good" | "up-bad" | "neutral";

export const KPI_TONE: Record<KpiMetric, KpiTone> = {
  traces: "neutral",
  error_rate: "up-bad",
  latency_p95: "up-bad",
  cost_total: "up-bad",
  total_tokens: "neutral",
};

/** A change of at least this many percent shows as a ratio (`×6`) instead. */
const RATIO_FROM_PCT = 500;
const SPARK_HEIGHT = 20;
const TILES: { metric: KpiMetric; label: string }[] = [
  { metric: "traces", label: "Traces" },
  { metric: "error_rate", label: "Error rate" },
  { metric: "latency_p95", label: "p95 latency" },
  { metric: "cost_total", label: "Cost" },
  { metric: "total_tokens", label: "Tokens" },
];

export function KpiRow({ kpis }: { kpis: readonly Schema<"Kpi">[] }) {
  return (
    <div className="grid grid-cols-2 gap-2 md:grid-cols-5">
      {TILES.map((tile) => (
        <KpiTile
          key={tile.metric}
          tile={tile}
          kpi={kpis.find((item) => item.metric === tile.metric)}
        />
      ))}
    </div>
  );
}

/** The KPI row while its data loads: each tile's label, with a bar where the value goes. */
export function KpiRowSkeleton() {
  return (
    <div className="grid grid-cols-2 gap-2 md:grid-cols-5">
      {TILES.map((tile) => (
        <div key={tile.metric} className="flex h-16 flex-col justify-center rounded-md border px-3">
          <span className="text-muted-foreground text-caption">{tile.label}</span>
          <Skeleton className="mt-1 h-4 w-16" />
        </div>
      ))}
    </div>
  );
}

function KpiTile({ tile, kpi }: { tile: (typeof TILES)[number]; kpi?: Schema<"Kpi"> }) {
  const delta = kpiDelta(kpi ?? {});
  const value = delta.value === undefined ? "—" : formatKpi(tile.metric, delta.value);
  const previous =
    delta.previous === undefined ? "no data" : formatKpi(tile.metric, delta.previous);
  return (
    <Tooltip>
      <TooltipTrigger
        type="button"
        className="focus-ring flex min-h-16 flex-col justify-center rounded-md border px-3 py-1 text-left"
        aria-label={`${tile.label}: ${value}`}
      >
        <span className="text-muted-foreground text-caption">{tile.label}</span>
        <span className="flex items-baseline gap-2">
          <span className="font-mono text-body tabular-nums">{value}</span>
          <Delta tone={KPI_TONE[tile.metric]} delta={delta} />
        </span>
      </TooltipTrigger>
      <TooltipContent>Previous period: {previous}</TooltipContent>
    </Tooltip>
  );
}

export function Delta({ tone, delta }: { tone: KpiTone; delta: KpiDelta }) {
  const Icon = delta.direction === "up" ? ArrowUp : delta.direction === "down" ? ArrowDown : Minus;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-0.5 text-caption",
        deltaClass(tone, delta.isNew ? "up" : delta.direction),
      )}
    >
      {delta.isNew ? null : <Icon aria-hidden className="size-3" />}
      {deltaText(delta)}
    </span>
  );
}

/** The badge text: `new`, a ratio for large changes, otherwise the percentage. `null` shows no text. */
function deltaText(delta: KpiDelta): string | null {
  if (delta.isNew) {
    return "new";
  }
  if (delta.changePct === undefined) {
    return null;
  }
  if (Math.abs(delta.changePct) >= RATIO_FROM_PCT && delta.value !== undefined && delta.previous) {
    return `×${formatChange(delta.value / delta.previous)}`;
  }
  return `${formatChange(delta.changePct)}%`;
}

function deltaClass(tone: KpiTone, direction: KpiDirection): string {
  if (tone === "neutral" || direction === "flat") {
    return "text-muted-foreground";
  }
  const good = tone === "up-good" ? direction === "up" : direction === "down";
  return good ? "text-chart-2" : "text-destructive";
}

function formatChange(changePct: number): string {
  const abs = Math.abs(changePct);
  const rounded = abs >= 10 ? Math.round(abs) : Math.round(abs * 10) / 10;
  return Number.isInteger(rounded) ? String(rounded) : rounded.toFixed(1);
}

/**
 * A small line under the value. Gaps break the line. Nothing is drawn with
 * fewer than two present values.
 */
export function Sparkline({ values }: { values?: readonly (number | null)[] }) {
  if (!values) {
    return null;
  }
  const present = values.filter(isNumber);
  if (present.length < 2) {
    return null;
  }
  const min = Math.min(...present);
  const span = Math.max(...present) - min;
  const last = values.length - 1;
  const runs: string[][] = [];
  let run: string[] = [];
  values.forEach((value, index) => {
    if (!isNumber(value)) {
      runs.push(run);
      run = [];
      return;
    }
    const x = last === 0 ? 0 : (index / last) * 100;
    const y =
      span === 0
        ? SPARK_HEIGHT / 2
        : SPARK_HEIGHT - 2 - ((value - min) / span) * (SPARK_HEIGHT - 4);
    run.push(`${x.toFixed(2)},${y.toFixed(2)}`);
  });
  runs.push(run);
  return (
    <svg
      aria-hidden="true"
      className="text-muted-foreground mt-1 block h-5 w-full"
      viewBox={`0 0 100 ${SPARK_HEIGHT}`}
      preserveAspectRatio="none"
    >
      {runs
        .filter((points) => points.length >= 2)
        .map((points) => (
          <polyline
            key={points[0]}
            points={points.join(" ")}
            fill="none"
            stroke="currentColor"
            strokeWidth={1.5}
            vectorEffect="non-scaling-stroke"
          />
        ))}
    </svg>
  );
}

function isNumber(value: number | null | undefined): value is number {
  return typeof value === "number" && Number.isFinite(value);
}
