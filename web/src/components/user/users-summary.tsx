import type { Schema } from "@/api/types";
import { Delta, KPI_TONE, type KpiTone } from "@/components/charts/kpi-tiles";
import { formatKpi, type KpiDelta, type KpiMetric, kpiDelta } from "@/lib/metrics";

const ITEMS: { label: string; metric: KpiMetric }[] = [
  { label: "Traces", metric: "traces" },
  { label: "Error rate", metric: "error_rate" },
  { label: "p95 latency", metric: "latency_p95" },
  { label: "Cost", metric: "cost_total" },
  { label: "Tokens", metric: "total_tokens" },
];

export function UsersSummary({
  kpis,
  users,
  more,
}: {
  kpis: readonly Schema<"Kpi">[] | undefined;
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
            tone={KPI_TONE[item.metric]}
            previous={delta.previous === undefined ? "—" : formatKpi(item.metric, delta.previous)}
          />
        );
      })}
    </section>
  );
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
