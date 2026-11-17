import { memo, useMemo, useState } from "react";
import { Bar, CartesianGrid, ComposedChart, Legend, Line, XAxis, YAxis } from "recharts";
import type { Schema } from "@/api/types";
import { ChartSettings } from "@/components/sql/chart-settings";
import { ChartContainer, ChartTooltip, ChartTooltipContent } from "@/components/ui/chart";
import { formatTimestamp } from "@/lib/format";
import { useTimeZone } from "@/lib/settings";
import {
  autoChartConfig,
  buildChart,
  CHART_MAX_POINTS,
  CHART_MAX_SERIES,
  type ChartConfig,
  type ChartSpec,
  chartColumns,
  chartLabel,
  X_KEY,
} from "@/lib/sql/chart";

const COLORS = [
  "var(--chart-1)",
  "var(--chart-2)",
  "var(--chart-3)",
  "var(--chart-4)",
  "var(--chart-5)",
];
const DAY_MS = 86_400_000;

export const NO_CHART =
  "No chart for this result: it needs a time or text column and a numeric column.";
export const CLIPPED = `Showing the first ${CHART_MAX_POINTS} rows and at most ${CHART_MAX_SERIES} series.`;

/**
 * The config a result starts from: the automatic choice, else a bar of the first x column and
 * the first y column that is not x. Null when there is no usable x or y.
 */
function defaultConfig(result: Schema<"SqlQueryResult">): ChartConfig | null {
  const automatic = autoChartConfig(result);
  if (automatic) {
    return automatic;
  }
  const columns = chartColumns(result);
  const x = columns.x[0];
  const y = columns.y.find((name) => name !== x);
  if (x === undefined || y === undefined) {
    return null;
  }
  return { kind: "bar", x, y: [y], splitBy: null };
}

/** `time` for a timestamp axis under a day long, `datetime` otherwise. */
function tickStyle(spec: ChartSpec): "time" | "datetime" {
  const first = Date.parse(String(spec.data[0]?.[X_KEY] ?? ""));
  const last = Date.parse(String(spec.data.at(-1)?.[X_KEY] ?? ""));
  return Number.isFinite(first) && Number.isFinite(last) && last - first < DAY_MS
    ? "time"
    : "datetime";
}

export const SqlResultChart = memo(function SqlResultChart({
  result,
}: {
  result: Schema<"SqlQueryResult">;
}) {
  const zone = useTimeZone();
  const automatic = useMemo(() => defaultConfig(result), [result]);
  const columns = useMemo(() => chartColumns(result), [result]);
  // The user's choice belongs to this result: a new result starts again from the default.
  const [chosen, setChosen] = useState<
    { result: Schema<"SqlQueryResult">; config: ChartConfig } | undefined
  >();
  const config = chosen?.result === result ? chosen.config : automatic;
  const spec = useMemo(() => (config ? buildChart(result, config) : null), [result, config]);
  if (!config) {
    return <p className="text-muted-foreground text-sm">{NO_CHART}</p>;
  }
  const style = spec ? tickStyle(spec) : "datetime";
  const tick = (value: unknown) =>
    spec?.x.kind === "timestamp" ? formatTimestamp(String(value), zone, style) : String(value);
  return (
    <section
      aria-label={spec ? chartLabel(spec) : "Chart"}
      className="flex min-h-0 flex-1 flex-col gap-1"
    >
      <ChartSettings
        config={config}
        columns={columns}
        onChange={(next) => setChosen({ result, config: next })}
      />
      {spec ? (
        <>
          {spec.clipped ? <p className="text-muted-foreground text-xs">{CLIPPED}</p> : null}
          <ChartContainer config={{}} className="aspect-auto min-h-64 w-full flex-1">
            <ComposedChart data={spec.data} margin={{ left: 8, right: 8, top: 8, bottom: 8 }}>
              <CartesianGrid vertical={false} />
              <XAxis dataKey={X_KEY} tickFormatter={tick} minTickGap={24} />
              <YAxis width={56} />
              <ChartTooltip
                content={<ChartTooltipContent labelFormatter={(value) => tick(value)} />}
              />
              <Legend />
              {spec.series.map((name, index) => {
                const color = COLORS[index % COLORS.length];
                return spec.kind === "line" ? (
                  <Line
                    key={name}
                    name={name}
                    dataKey={name}
                    type="monotone"
                    dot={false}
                    stroke={color}
                    isAnimationActive={false}
                    connectNulls
                  />
                ) : (
                  <Bar
                    key={name}
                    name={name}
                    dataKey={name}
                    fill={color}
                    isAnimationActive={false}
                  />
                );
              })}
            </ComposedChart>
          </ChartContainer>
        </>
      ) : (
        <p className="text-muted-foreground text-sm">{NO_CHART}</p>
      )}
    </section>
  );
});
