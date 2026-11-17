import type { Schema } from "@/api/types";
import { formatKpi, type KpiMetric, kpiDelta } from "@/lib/metrics";

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
      className="flex h-8 shrink-0 items-center gap-4 overflow-x-auto border-b px-3 text-xs"
    >
      <Item label="Users" value={`${users}${more ? "+" : ""}`} />
      {ITEMS.map((item) => {
        const kpi = kpis?.find((entry) => entry.metric === item.metric);
        const value = kpiDelta(kpi ?? {}).value;
        return (
          <Item
            key={item.metric}
            label={item.label}
            value={value === undefined ? "—" : formatKpi(item.metric, value)}
          />
        );
      })}
    </section>
  );
}

function Item({ label, value }: { label: string; value: string }) {
  return (
    <span className="shrink-0">
      <span className="text-muted-foreground">{label}</span> <span className="num">{value}</span>
    </span>
  );
}
