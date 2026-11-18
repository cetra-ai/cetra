import { Delta, type KpiTone, Sparkline } from "@/components/charts/kpi-tiles";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatValue, type MeasureKind } from "@/lib/dashboards/format";
import { kpiDelta } from "@/lib/metrics";

/**
 * A single value: its formatted number, the change against the previous period of the same length
 * (its tooltip gives that value) and a sparkline of the buckets.
 */
export function WidgetNumber({
  label,
  kind,
  value,
  previous,
  tone,
  sparkline,
}: {
  /** The widget's title and value, read by screen readers: "<title>: <value>". */
  label: string;
  kind: MeasureKind;
  value: number | null;
  previous: number | null;
  tone: KpiTone;
  sparkline?: readonly (number | null)[];
}) {
  const delta = kpiDelta({
    value: value ?? undefined,
    previous: previous ?? undefined,
  });
  return (
    <figure aria-label={label} className="m-0 flex min-h-0 flex-1 flex-col justify-center">
      <Tooltip>
        <TooltipTrigger
          type="button"
          className="focus-ring flex items-baseline gap-2 self-start rounded-sm text-left"
        >
          <span className="font-mono text-title tabular-nums">{formatValue(kind, value)}</span>
          {value === null ? null : <Delta tone={tone} delta={delta} />}
        </TooltipTrigger>
        <TooltipContent>Previous period: {formatValue(kind, previous)}</TooltipContent>
      </Tooltip>
      <Sparkline values={sparkline} />
    </figure>
  );
}
