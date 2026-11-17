import { cn } from "cn";
import type { Schema } from "@/api/types";
import { Skeleton } from "@/components/ui/skeleton";
import { Sparkline } from "@/components/user/sparkline";
import { formatCost, formatDuration } from "@/lib/format";

type EvaluatorStats = Schema<"EvaluatorStats">;
type EvaluatorJobStats = Schema<"EvaluatorJobStats">;
type EvaluatorScoreStats = Schema<"EvaluatorScoreStats">;

/** One evaluator's stats as a cell gets them: `pending` while the stats query is loading. */
export type StatsState = { stats: EvaluatorStats | undefined; pending: boolean };

/** A failed run share above this is shown as an error (5 %). */
export const ERROR_RATE_ALERT = 0.05;

/** `failed / (done + failed)`, or `undefined` when the evaluator has no finished run to judge. */
export function errorRate(jobs: EvaluatorJobStats): number | undefined {
  const judged = jobs.done + jobs.failed;
  return judged === 0 ? undefined : jobs.failed / judged;
}

export function errorRateTone(rate: number): string {
  return rate > ERROR_RATE_ALERT ? "text-destructive" : "text-muted-foreground";
}

/** The stats of each evaluator by id. */
export function statsById(data: EvaluatorStats[] | undefined): Map<string, EvaluatorStats> {
  return new Map((data ?? []).map((entry) => [entry.evaluator_id, entry]));
}

function Dash() {
  return <span className="text-[13px] text-muted-foreground">—</span>;
}

function Loading() {
  return <Skeleton className="h-4 w-12" />;
}

export function RunsCell({ stats, pending }: StatsState) {
  if (pending) {
    return <Loading />;
  }
  if (!stats) {
    return <Dash />;
  }
  const { done, failed, skipped } = stats.jobs;
  const total = done + failed + skipped;
  return (
    <div className="flex items-center gap-2">
      <Sparkline
        values={stats.jobs.runs_daily.map((point) => point.value ?? 0)}
        label="Runs per day, last 7 days"
      />
      <span
        className={cn(
          "text-[13px] tabular-nums",
          total === 0 ? "text-muted-foreground" : undefined,
        )}
      >
        {total}
      </span>
    </div>
  );
}

export function ErrorsCell({ stats, pending }: StatsState) {
  if (pending) {
    return <Loading />;
  }
  const rate = stats ? errorRate(stats.jobs) : undefined;
  if (!stats || rate === undefined) {
    return <Dash />;
  }
  const { done, failed } = stats.jobs;
  return (
    <span
      className={cn("text-[13px] tabular-nums", errorRateTone(rate))}
      title={`${failed} failed of ${done + failed} runs`}
    >
      {`${(rate * 100).toFixed(1)}%`}
    </span>
  );
}

export function AvgCostCell({ stats, pending }: StatsState) {
  if (pending) {
    return <Loading />;
  }
  if (!stats) {
    return <Dash />;
  }
  const cost = stats.jobs.cost_usd_avg;
  return cost === undefined ? (
    <Dash />
  ) : (
    <span className="text-[13px] tabular-nums">{formatCost(cost)}</span>
  );
}

export function LatencyP50Cell({ stats, pending }: StatsState) {
  if (pending) {
    return <Loading />;
  }
  const p50 = stats?.jobs.latency_p50_ms;
  if (p50 === undefined) {
    return <Dash />;
  }
  const p95 = stats?.jobs.latency_p95_ms;
  return (
    <span
      className="text-[13px] tabular-nums"
      title={p95 === undefined ? undefined : `p95 ${formatDuration(p95 * 1_000_000)}`}
    >
      {formatDuration(p50 * 1_000_000)}
    </span>
  );
}

/** The result of an evaluator's scores, as text: no sparkline. "—" when there is no value. */
export function resultText(scores: EvaluatorScoreStats): string {
  if (scores.count === 0) {
    return "—";
  }
  switch (scores.data_type) {
    case "numeric":
      return scores.average === undefined ? "—" : scores.average.toFixed(2);
    case "boolean":
      return scores.pass_rate === undefined ? "—" : `${Math.round(scores.pass_rate * 100)}% pass`;
    case "categorical":
      return scores.top_category === undefined
        ? "—"
        : `${scores.top_category.label} (${scores.top_category.count})`;
    case "text":
      return `${scores.count} scores`;
  }
}

export function ResultCell({ stats, pending }: StatsState) {
  if (pending) {
    return <Loading />;
  }
  if (!stats) {
    return <Dash />;
  }
  const text = resultText(stats.scores);
  const trend = stats.scores.data_type === "numeric" || stats.scores.data_type === "boolean";
  return (
    <div className="flex items-center gap-2 whitespace-nowrap">
      <span className="text-[13px] tabular-nums">{text}</span>
      {trend ? (
        <Sparkline
          values={stats.scores.daily.map((point) => point.value ?? 0)}
          label="Daily result, last 7 days"
        />
      ) : null}
    </div>
  );
}
