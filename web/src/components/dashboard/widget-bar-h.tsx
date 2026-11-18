import { Bar, BarChart, CartesianGrid, Cell, Tooltip, XAxis, YAxis } from "recharts";
import { AXIS_TICK } from "@/components/charts/plot-helpers";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { axisTicks, formatValue } from "@/lib/dashboards/format";
import type { TablePlot } from "@/lib/dashboards/plot";

type BarRow = { name: string; value: number; color: string; values: (number | null)[] };

/**
 * One horizontal bar per group, sized by the first measure. The tooltip lists every measure.
 * `colorOf` gives a group's colour (the model colours of the dashboard).
 */
export function WidgetBarH({
  plot,
  colorOf,
}: {
  plot: TablePlot;
  colorOf: (group: string | null | undefined) => string;
}) {
  const first = plot.columns[0];
  const rows: BarRow[] = plot.rows.map((row) => ({
    name: row.other ? "Other" : (row.group ?? "Total"),
    value: row.values[0] ?? 0,
    color: colorOf(row.group),
    values: row.values,
  }));
  const largest = rows.reduce((max, row) => Math.max(max, row.value), 0);
  const scale = axisTicks(first?.kind ?? "count", largest);
  const config: ChartConfig = { value: { label: first?.label ?? "" } };
  return (
    <ChartContainer
      config={config}
      initialDimension={{ width: 640, height: 160 }}
      className="aspect-auto min-h-0 w-full flex-1"
    >
      <BarChart data={rows} layout="vertical" margin={{ top: 4, right: 12, left: 0, bottom: 0 }}>
        <CartesianGrid stroke="var(--border)" strokeOpacity={0.7} horizontal={false} />
        <XAxis
          type="number"
          tick={AXIS_TICK}
          ticks={scale.ticks}
          domain={[0, scale.ticks.at(-1) ?? 1]}
          tickFormatter={(value) => scale.format(Number(value))}
          axisLine={false}
          tickLine={false}
        />
        <YAxis
          type="category"
          dataKey="name"
          width={112}
          tick={AXIS_TICK}
          axisLine={false}
          tickLine={false}
        />
        <Tooltip
          cursor={{ fill: "var(--muted)" }}
          content={(props) => (
            <BarTooltip
              active={props.active}
              payload={props.payload as ReadonlyArray<{ payload?: BarRow }> | undefined}
              columns={plot.columns}
            />
          )}
        />
        <Bar dataKey="value" isAnimationActive={false}>
          {rows.map((row) => (
            <Cell key={row.name} fill={row.color} />
          ))}
        </Bar>
      </BarChart>
    </ChartContainer>
  );
}

function BarTooltip({
  active,
  payload,
  columns,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: BarRow }>;
  columns: TablePlot["columns"];
}) {
  const row = payload?.[0]?.payload;
  if (!active || !row) {
    return null;
  }
  return (
    <div className="grid min-w-32 gap-1 rounded-md border bg-popover px-2.5 py-1.5 text-caption shadow-md">
      <div className="font-mono text-muted-foreground">{row.name}</div>
      {columns.map((column, index) => (
        <div key={column.label} className="flex items-center justify-between gap-3">
          <span className="text-muted-foreground">{column.label}</span>
          <span className="font-mono text-foreground tabular-nums">
            {formatValue(column.kind, row.values[index] ?? null)}
          </span>
        </div>
      ))}
    </div>
  );
}
