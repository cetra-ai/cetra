import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import type { BarShapeProps } from "recharts";
import { Bar, BarChart, Tooltip, XAxis, YAxis } from "recharts";
import { orgUsageQuery } from "@/api/queries-settings";
import { BucketBar, roundsTop } from "@/components/charts/plot-helpers";
import { numbers, UsageMeter } from "@/components/plan/usage-meter";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { Skeleton } from "@/components/ui/skeleton";
import { formatCost } from "@/lib/format";

const DAY_MS = 86_400_000;

/** One UTC day of the strip's chart: `units` is null on a day without units (no bar). */
export type DayRow = { day: string; time: number; units: number | null };

/** Every UTC day from `usage.from` to `usage.to`, inclusive, with its units. */
export function usageDayRows(usage: {
  from: string;
  to: string;
  days: readonly { day: string; units: number }[];
}): DayRow[] {
  const start = Date.parse(`${usage.from}T00:00:00Z`);
  const end = Date.parse(`${usage.to}T00:00:00Z`);
  if (Number.isNaN(start) || Number.isNaN(end) || end < start) {
    return [];
  }
  const units = new Map(usage.days.map((row) => [row.day, row.units]));
  const rows: DayRow[] = [];
  for (let time = start; time <= end; time += DAY_MS) {
    const day = new Date(time).toISOString().slice(0, 10);
    const value = units.get(day) ?? 0;
    rows.push({ day, time, units: value > 0 ? value : null });
  }
  return rows;
}

/** The month's spans, scores and judge cost, summed over `days`. */
export function usageTotals(
  days: readonly { spans: number; scores: number; judge_cost_usd: number }[],
): { spans: number; scores: number; judgeCost: number } {
  return days.reduce(
    (totals, day) => ({
      spans: totals.spans + day.spans,
      scores: totals.scores + day.scores,
      judgeCost: totals.judgeCost + day.judge_cost_usd,
    }),
    { spans: 0, scores: 0, judgeCost: 0 },
  );
}

const UNITS_CONFIG = {
  units: { label: "Units", color: "var(--chart-1)" },
} satisfies ChartConfig;

/** The org's usage this month (D-C-27). Hidden when either of its requests fails (OH-9). */
export function UsageStrip({ org }: { org: string }) {
  const { query: entitlements } = useOrgEntitlements(org);
  const usage = useQuery(orgUsageQuery(org));

  if (entitlements.isError || usage.isError) {
    return null;
  }
  if (entitlements.data === undefined || usage.data === undefined) {
    return <Skeleton className="h-20 w-full" />;
  }

  const month = entitlements.data.usage;
  const limits = entitlements.data.limits;
  const rows = usageDayRows(usage.data);
  const totals = usageTotals(usage.data.days);

  return (
    <section
      aria-label="Usage this month"
      className="grid gap-4 rounded-md border p-3 md:grid-cols-[minmax(0,16rem)_auto_minmax(0,1fr)] md:items-center"
    >
      <div className="flex flex-col gap-1">
        <div className="flex items-center justify-between gap-2">
          <span className="text-caption font-medium text-muted-foreground">Usage this month</span>
          <Link
            to="/$org/settings/plan"
            params={{ org }}
            className="focus-ring rounded-sm text-caption text-muted-foreground hover:text-foreground hover:underline"
          >
            Plan
          </Link>
        </div>
        <UsageMeter
          variant="compact"
          units={month.units}
          resetsAt={month.resets_at}
          included={limits.units_included}
          cap={limits.units_cap}
        />
      </div>
      <dl className="flex gap-4">
        <div className="flex flex-col">
          <dt className="text-caption text-muted-foreground">Spans</dt>
          <dd className="text-body tabular-nums">{numbers.format(totals.spans)}</dd>
        </div>
        <div className="flex flex-col">
          <dt className="text-caption text-muted-foreground">Scores</dt>
          <dd className="text-body tabular-nums">{numbers.format(totals.scores)}</dd>
        </div>
        <div className="flex flex-col">
          <dt className="text-caption text-muted-foreground">Judge cost</dt>
          <dd className="text-body tabular-nums">{formatCost(totals.judgeCost)}</dd>
        </div>
      </dl>
      {rows.some((row) => row.units !== null) ? (
        <DailyUnitsChart rows={rows} />
      ) : (
        <p className="text-body text-muted-foreground">No usage this month.</p>
      )}
    </section>
  );
}

function DailyUnitsChart({ rows }: { rows: readonly DayRow[] }) {
  return (
    <div role="img" aria-label="Daily units" className="h-12 w-full select-none">
      <ChartContainer
        config={UNITS_CONFIG}
        initialDimension={{ width: 320, height: 48 }}
        className="aspect-auto h-12 w-full"
      >
        <BarChart
          data={[...rows]}
          barCategoryGap="20%"
          margin={{ top: 2, right: 0, bottom: 0, left: 0 }}
          accessibilityLayer={false}
        >
          <XAxis dataKey="time" hide />
          <YAxis hide domain={[0, "auto"]} />
          <Tooltip cursor={false} content={<UnitsTooltip />} />
          <Bar
            dataKey="units"
            fill="var(--chart-1)"
            isAnimationActive={false}
            shape={(props: BarShapeProps) => (
              <BucketBar {...props} roundTop={roundsTop(props.payload, "units")} />
            )}
          />
        </BarChart>
      </ChartContainer>
    </div>
  );
}

function UnitsTooltip({
  active,
  payload,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: DayRow }>;
}) {
  const row = payload?.[0]?.payload;
  if (!active || !row) {
    return null;
  }
  return (
    <div className="grid min-w-32 gap-1 rounded-md border bg-popover px-2.5 py-1.5 text-caption shadow-md">
      <div className="font-mono text-caption text-muted-foreground">{row.day}</div>
      <div className="flex items-center justify-between gap-4">
        <span className="text-muted-foreground">Units</span>
        <span className="font-mono tabular-nums">{numbers.format(row.units ?? 0)}</span>
      </div>
    </div>
  );
}
