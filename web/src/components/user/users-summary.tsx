import { widgetQuery } from "@/api/dashboards";
import type { Schema } from "@/api/types";
import { Delta, type KpiTone } from "@/components/charts/kpi-tiles";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import { formatCount, formatPercent, type KpiDelta, kpiDelta } from "@/lib/metrics";

/** The headline numbers of the users list, over the list's range and filter. */
export type UsersKpiMetric =
  | "traces"
  | "error_rate"
  | "latency_p95"
  | "cost_total"
  | "total_tokens";

/** One headline number: its value over the range and over the previous range of the same length. */
export type UsersKpi = { metric: UsersKpiMetric; value?: number; previous?: number };

const ITEMS: { label: string; metric: UsersKpiMetric }[] = [
  { label: "Traces", metric: "traces" },
  { label: "Error rate", metric: "error_rate" },
  { label: "p95 latency", metric: "latency_p95" },
  { label: "Cost", metric: "cost_total" },
  { label: "Tokens", metric: "total_tokens" },
];

/** What a rise in a number means: a rise of an error rate, a latency or a cost is bad news. */
const TONE: Record<UsersKpiMetric, KpiTone> = {
  traces: "neutral",
  error_rate: "up-bad",
  latency_p95: "up-bad",
  cost_total: "up-bad",
  total_tokens: "neutral",
};

/**
 * The widget query of the headline numbers: one total with a measure per number, and the previous
 * range. Each number is the same as the one the users list has always shown.
 */
export function usersKpiQuery(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    filter?: string;
    includeEval: boolean;
    includeExperiments: boolean;
  },
) {
  return widgetQuery(org, project, {
    from: query.from,
    to: query.to,
    shape: "total",
    compare: true,
    ...(query.filter ? { filter: query.filter } : {}),
    include_eval: query.includeEval,
    include_experiments: query.includeExperiments,
    query: {
      source: "traces",
      measures: [
        { agg: "count" },
        { agg: "rate", where: "status = 'error'" },
        { agg: "p95", field: "duration_ns" },
        { agg: "sum", field: "cost_total" },
        { agg: "sum", field: "total_tokens" },
      ],
    },
  });
}

/** The headline numbers of a `usersKpiQuery` answer, one per metric. */
export function usersKpis(
  answer: Schema<"WidgetQueryResponse"> | undefined,
): UsersKpi[] | undefined {
  const row = answer?.shape === "total" ? answer.rows[0] : undefined;
  if (row === undefined) {
    return undefined;
  }
  return ITEMS.map((item, index) => ({
    metric: item.metric,
    value: row.values[index] ?? undefined,
    previous: row.previous?.[index] ?? undefined,
  }));
}

export function UsersSummary({
  kpis,
  users,
  more,
}: {
  kpis: readonly UsersKpi[] | undefined;
  users: number;
  more: boolean;
}) {
  return (
    <section
      aria-label="Summary"
      className="flex h-8 shrink-0 items-center gap-4 overflow-x-auto border-b px-3 text-caption"
    >
      <Item label="Users" value={`${users}${more ? "+" : ""}`} />
      {ITEMS.map((item) => {
        const kpi = kpis?.find((entry) => entry.metric === item.metric);
        const delta = kpiDelta(kpi ?? {});
        return (
          <Item
            key={item.metric}
            label={item.label}
            value={delta.value === undefined ? "—" : formatKpi(item.metric, delta.value)}
            delta={delta}
            tone={TONE[item.metric]}
            previous={delta.previous === undefined ? "—" : formatKpi(item.metric, delta.previous)}
          />
        );
      })}
    </section>
  );
}

function formatKpi(metric: UsersKpiMetric, value: number): string {
  switch (metric) {
    case "traces":
      return formatCount(value);
    case "error_rate":
      return formatPercent(value);
    case "latency_p95":
      return formatDuration(value);
    case "cost_total":
      return formatCost(value);
    case "total_tokens":
      return formatTokens(value);
  }
}

function Item({
  label,
  value,
  delta,
  tone,
  previous,
}: {
  label: string;
  value: string;
  delta?: KpiDelta;
  tone?: KpiTone;
  previous?: string;
}) {
  return (
    <span className="shrink-0" title={previous === undefined ? undefined : `Previous: ${previous}`}>
      <span className="text-muted-foreground">{label}</span> <span className="num">{value}</span>
      {delta && tone !== undefined ? (
        <>
          {" "}
          <Delta tone={tone} delta={delta} />
        </>
      ) : null}
    </span>
  );
}
