import { cn } from "cn";
import { ArrowDown, ArrowUp, Minus } from "lucide-react";
import type { Schema } from "@/api/types";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatKpi, type KpiDirection, type KpiMetric, kpiDelta } from "@/lib/metrics";

/** Whether a rise in the metric is bad news: the tile's delta colour flips. */
export const KPI_ADVERSE: Record<KpiMetric, boolean> = {
  traces: false,
  error_rate: true,
  latency_p95: true,
  cost_total: true,
  total_tokens: false,
};

const TILES: { metric: KpiMetric; label: string; adverse: boolean }[] = [
  { metric: "traces", label: "Traces", adverse: KPI_ADVERSE.traces },
  { metric: "error_rate", label: "Error rate", adverse: KPI_ADVERSE.error_rate },
  { metric: "latency_p95", label: "p95 latency", adverse: KPI_ADVERSE.latency_p95 },
  { metric: "cost_total", label: "Cost", adverse: KPI_ADVERSE.cost_total },
  { metric: "total_tokens", label: "Tokens", adverse: KPI_ADVERSE.total_tokens },
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
          <span className="text-muted-foreground text-xs">{tile.label}</span>
          <Skeleton className="mt-1 h-4 w-16" />
        </div>
      ))}
    </div>
  );
}

function KpiTile({ tile, kpi }: { tile: (typeof TILES)[number]; kpi?: Schema<"Kpi"> }) {
  const delta = kpiDelta(kpi ?? {});
  const previous = delta.previous === undefined ? "—" : formatKpi(tile.metric, delta.previous);
  return (
    <Tooltip>
      <TooltipTrigger
        type="button"
        className="focus-ring flex h-16 flex-col justify-center rounded-md border px-3 text-left"
        aria-label={`${tile.label}: ${delta.value === undefined ? "—" : formatKpi(tile.metric, delta.value)}`}
      >
        <span className="text-muted-foreground text-xs">{tile.label}</span>
        <span className="flex items-baseline gap-2">
          <span className="font-mono text-sm tabular-nums">
            {delta.value === undefined ? "—" : formatKpi(tile.metric, delta.value)}
          </span>
          <Delta adverse={tile.adverse} direction={delta.direction} changePct={delta.changePct} />
        </span>
      </TooltipTrigger>
      <TooltipContent>Previous: {previous}</TooltipContent>
    </Tooltip>
  );
}

export function Delta({
  adverse,
  direction,
  changePct,
}: {
  adverse: boolean;
  direction: KpiDirection;
  changePct?: number;
}) {
  const Icon = direction === "up" ? ArrowUp : direction === "down" ? ArrowDown : Minus;
  return (
    <span
      className={cn("inline-flex items-center gap-0.5 text-xs", deltaClass(adverse, direction))}
    >
      <Icon aria-hidden className="size-3" />
      {changePct === undefined ? null : `${formatChange(changePct)}%`}
    </span>
  );
}

function deltaClass(adverse: boolean, direction: KpiDirection): string {
  if (direction === "flat") {
    return "text-muted-foreground";
  }
  const improved = adverse ? direction === "down" : direction === "up";
  return improved ? "text-chart-2" : "text-destructive";
}

function formatChange(changePct: number): string {
  const abs = Math.abs(changePct);
  const rounded = abs >= 10 ? Math.round(abs) : Math.round(abs * 10) / 10;
  return Number.isInteger(rounded) ? String(rounded) : rounded.toFixed(1);
}
