import { ChevronDown, ChevronRight } from "lucide-react";
import { useState } from "react";
import { CartesianGrid, Line, LineChart, Tooltip, XAxis, YAxis } from "recharts";
import type { Schema } from "@/api/types";
import { TimeText } from "@/components/time-text";
import {
  type ChartConfig,
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
} from "@/components/ui/chart";
import { headlineValue } from "@/lib/experiments";
import { PALETTE } from "@/lib/metrics";
import { STORAGE_PREFIX } from "@/product";

/** A run with its summary, oldest first. */
export type TrendRun = {
  experiment: Schema<"Experiment">;
  summary: Schema<"ExperimentSummary">;
};

type Series = { key: string; name: string; dataType: "numeric" | "boolean" };
type TrendRow = { id: string; label: string; created: string } & Record<string, unknown>;

const OPEN_KEY = `${STORAGE_PREFIX}experiments.trend`;
const AXIS_TICK = { fontSize: 11, fill: "var(--muted-foreground)" };

function readOpen(): boolean {
  try {
    return localStorage.getItem(OPEN_KEY) !== "closed";
  } catch {
    return true;
  }
}

function writeOpen(open: boolean) {
  try {
    localStorage.setItem(OPEN_KEY, open ? "open" : "closed");
  } catch {
    // Storage can be blocked; the section then opens by default each time.
  }
}

/**
 * The headline score of each run over time, one line per numeric or boolean score: numeric by its
 * mean on the left axis, boolean by its pass rate in % on the right. A click on a point opens the
 * run through `onOpen`. `runs` are oldest first.
 */
export function ExperimentsTrend({
  runs,
  onOpen,
}: {
  runs: TrendRun[];
  onOpen: (id: string) => void;
}) {
  const [open, setOpen] = useState(readOpen);

  const series: Series[] = [];
  for (const run of runs) {
    for (const score of run.summary.scores) {
      if (
        (score.data_type === "numeric" || score.data_type === "boolean") &&
        headlineValue(score) !== undefined &&
        !series.some((candidate) => candidate.name === score.name)
      ) {
        series.push({ key: `s${series.length}`, name: score.name, dataType: score.data_type });
      }
    }
  }
  const config: ChartConfig = Object.fromEntries(
    series.map((entry, index) => [
      entry.key,
      { label: entry.name, color: PALETTE[index % PALETTE.length] },
    ]),
  );
  const data: TrendRow[] = runs.map((run) => {
    const row: TrendRow = {
      id: run.experiment.id,
      label: run.experiment.name,
      created: run.experiment.created_at,
    };
    for (const entry of series) {
      const score = run.summary.scores.find((candidate) => candidate.name === entry.name);
      const value = score === undefined ? undefined : headlineValue(score);
      if (score?.data_type === entry.dataType && value !== undefined) {
        row[entry.key] = entry.dataType === "boolean" ? value * 100 : value;
      }
    }
    return row;
  });
  const hasNumeric = series.some((entry) => entry.dataType === "numeric");
  const hasBoolean = series.some((entry) => entry.dataType === "boolean");

  return (
    <section aria-label="Trend" className="flex flex-col gap-2 rounded-md border px-3 py-2">
      <button
        type="button"
        aria-expanded={open}
        className="flex w-fit items-center gap-1 text-body font-medium"
        onClick={() => {
          setOpen(!open);
          writeOpen(!open);
        }}
      >
        {open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
        Trend
      </button>
      {open ? (
        <ChartContainer
          config={config}
          initialDimension={{ width: 640, height: 160 }}
          className="aspect-auto h-[180px] w-full"
        >
          <LineChart
            data={data}
            margin={{ top: 8, right: 12, left: 0, bottom: 0 }}
            onClick={(state) => {
              const index = state?.activeTooltipIndex;
              const run = typeof index === "number" ? data[index] : undefined;
              if (run !== undefined) {
                onOpen(run.id);
              }
            }}
          >
            <CartesianGrid stroke="var(--border)" strokeOpacity={0.7} vertical={false} />
            <XAxis
              dataKey="label"
              tick={AXIS_TICK}
              minTickGap={28}
              axisLine={false}
              tickLine={false}
            />
            <YAxis
              yAxisId="num"
              hide={!hasNumeric}
              width={48}
              tick={AXIS_TICK}
              axisLine={false}
              tickLine={false}
            />
            <YAxis
              yAxisId="pct"
              orientation="right"
              hide={!hasBoolean}
              domain={[0, 100]}
              width={48}
              tick={AXIS_TICK}
              tickFormatter={(value) => `${value}%`}
              axisLine={false}
              tickLine={false}
            />
            <Tooltip
              content={({ active, payload }) => {
                const row = payload?.[0]?.payload as TrendRow | undefined;
                if (!active || row === undefined) {
                  return null;
                }
                return (
                  <div className="rounded-md border bg-popover px-2 py-1.5 text-body text-popover-foreground shadow-md">
                    <div className="font-medium">{row.label}</div>
                    <div className="text-muted-foreground">
                      <TimeText value={row.created} style="datetime" />
                    </div>
                    {series.map((entry) =>
                      typeof row[entry.key] === "number" ? (
                        <div key={entry.key} className="tabular-nums">
                          {`${entry.name} ${
                            entry.dataType === "boolean"
                              ? `${(row[entry.key] as number).toFixed(1)} %`
                              : (row[entry.key] as number).toFixed(3)
                          }`}
                        </div>
                      ) : null,
                    )}
                  </div>
                );
              }}
            />
            <ChartLegend verticalAlign="top" height={20} content={<ChartLegendContent />} />
            {series.map((entry) => (
              <Line
                key={entry.key}
                dataKey={entry.key}
                name={entry.name}
                yAxisId={entry.dataType === "boolean" ? "pct" : "num"}
                stroke={`var(--color-${entry.key})`}
                strokeWidth={1.5}
                dot={{ r: 3 }}
                connectNulls={false}
                isAnimationActive={false}
                type="linear"
              />
            ))}
          </LineChart>
        </ChartContainer>
      ) : null}
    </section>
  );
}
