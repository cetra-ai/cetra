import type { ReactNode } from "react";
import type { Schema } from "@/api/types";
import { ScoreDistribution } from "@/components/experiments/score-distribution";
import { headlineText, scoreSummaryText } from "@/lib/experiments";
import { formatCost, formatDuration } from "@/lib/format";

/** The summary of an experiment: its counts, latency and cost, then one card per score. */
export function ScoreSummaryCards({ summary }: { summary: Schema<"ExperimentSummary"> }) {
  const expected = summary.experiment.item_count * summary.experiment.repetitions;
  return (
    <ul aria-label="Summary" className="grid grid-cols-2 gap-3 md:grid-cols-4">
      <Card title="Results" text={`${summary.result_count} of ${expected}`} />
      <Card
        title="Errors"
        text={String(summary.error_count)}
        tone={summary.error_count > 0 ? "error" : undefined}
      />
      <Card title="Latency" text={latencyText(summary)} />
      <Card title="Cost" text={formatCost(summary.cost_total)} />
      {summary.scores.map((score) => (
        <Card
          key={score.name}
          title={score.name}
          text={headlineText(score)}
          detail={scoreSummaryText(score)}
          chart={<ScoreDistribution summary={score} />}
        />
      ))}
    </ul>
  );
}

/** "p50 X · p95 Y", each left out when the API has no value for it. */
function latencyText(summary: Schema<"ExperimentSummary">): string {
  const parts: string[] = [];
  if (summary.latency_p50_ns !== undefined) {
    parts.push(`p50 ${formatDuration(summary.latency_p50_ns)}`);
  }
  if (summary.latency_p95_ns !== undefined) {
    parts.push(`p95 ${formatDuration(summary.latency_p95_ns)}`);
  }
  return parts.length === 0 ? "—" : parts.join(" · ");
}

function Card({
  title,
  text,
  detail,
  chart,
  tone,
}: {
  title: string;
  text: string;
  /** The full statistics line, under the headline in muted text. */
  detail?: string;
  chart?: ReactNode;
  tone?: "error";
}) {
  return (
    <li className="flex flex-col gap-1 rounded-md border p-3">
      <span className="text-body text-muted-foreground">{title}</span>
      <span
        className={
          tone === "error" ? "text-title tabular-nums text-status-error" : "text-title tabular-nums"
        }
      >
        {text}
      </span>
      {detail === undefined ? null : (
        <span className="text-caption text-muted-foreground tabular-nums">{detail}</span>
      )}
      {chart === undefined ? null : <div className="pt-1">{chart}</div>}
    </li>
  );
}
