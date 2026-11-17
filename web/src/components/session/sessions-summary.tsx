import type { Schema } from "@/api/types";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import { formatCount, formatPercent } from "@/lib/metrics";

type Session = Schema<"SessionSummary">;

/**
 * KPIs over the loaded session rows. The session list filters sessions, so the
 * metrics endpoint cannot reproduce these numbers.
 */
export type SessionStats = {
  sessions: number;
  avgTurns?: number;
  avgDurationNs?: number;
  errorRate?: number;
  cost?: number;
  tokens?: number;
};

export function sessionStats(sessions: readonly Session[]): SessionStats {
  if (sessions.length === 0) {
    return { sessions: 0 };
  }
  let durationNs = 0;
  let traces = 0;
  let errorTraces = 0;
  let cost: number | undefined;
  let tokens: number | undefined;
  for (const session of sessions) {
    durationNs += session.duration_ns;
    traces += session.trace_count;
    errorTraces += session.error_trace_count;
    if (session.cost_total !== undefined) {
      cost = (cost ?? 0) + session.cost_total;
    }
    if (session.total_tokens !== undefined) {
      tokens = (tokens ?? 0) + session.total_tokens;
    }
  }
  return {
    sessions: sessions.length,
    avgTurns: traces / sessions.length,
    avgDurationNs: durationNs / sessions.length,
    errorRate: traces > 0 ? errorTraces / traces : undefined,
    cost,
    tokens,
  };
}

export function SessionsSummary({
  sessions,
  more,
}: {
  sessions: readonly Session[];
  more: boolean;
}) {
  const stats = sessionStats(sessions);
  const loaded = sessions.length;
  return (
    <section
      aria-label="Summary"
      className="flex h-8 shrink-0 items-center gap-4 overflow-x-auto border-b px-3 text-xs"
    >
      <Item label="Sessions" value={`${formatCount(stats.sessions)}${more ? "+" : ""}`} />
      {more ? (
        <span className="shrink-0 text-muted-foreground">of {formatCount(loaded)} loaded</span>
      ) : null}
      <Item
        label="Avg turns"
        value={stats.avgTurns === undefined ? "—" : formatAverage(stats.avgTurns)}
      />
      <Item
        label="Avg duration"
        value={stats.avgDurationNs === undefined ? "—" : formatDuration(stats.avgDurationNs)}
      />
      <Item
        label="Error rate"
        value={stats.errorRate === undefined ? "—" : formatPercent(stats.errorRate)}
      />
      <Item label="Cost" value={formatCost(stats.cost)} />
      <Item label="Tokens" value={formatTokens(stats.tokens)} />
    </section>
  );
}

function formatAverage(value: number): string {
  return new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 }).format(value);
}

function Item({ label, value }: { label: string; value: string }) {
  return (
    <span className="shrink-0">
      <span className="text-muted-foreground">{label}</span> <span className="num">{value}</span>
    </span>
  );
}
