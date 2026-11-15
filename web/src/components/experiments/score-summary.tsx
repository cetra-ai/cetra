import type { Schema } from "@/api/types";
import { scoreSummaryText } from "@/lib/experiments";
import { formatCost, formatDuration } from "@/lib/format";

/** The summary of an experiment: its counts, latency and cost, then one card per score. */
export function ScoreSummaryCards({ summary }: { summary: Schema<"ExperimentSummary"> }) {
  const expected = summary.experiment.item_count * summary.experiment.repetitions;
  const latency =
    summary.latency_mean_ns === undefined || summary.latency_p95_ns === undefined
      ? "—"
      : `mean ${formatDuration(summary.latency_mean_ns)} · p95 ${formatDuration(summary.latency_p95_ns)}`;
  return (
    <ul aria-label="Summary" className="grid grid-cols-2 gap-3 md:grid-cols-4">
      <Card title="Results" text={`${summary.result_count} of ${expected}`} />
      <Card
        title="Errors"
        text={String(summary.error_count)}
        tone={summary.error_count > 0 ? "error" : undefined}
      />
      <Card title="Latency" text={latency} />
      <Card title="Cost" text={formatCost(summary.cost_total)} />
      {summary.scores.map((score) => (
        <Card key={score.name} title={score.name} text={scoreSummaryText(score)} />
      ))}
    </ul>
  );
}

function Card({ title, text, tone }: { title: string; text: string; tone?: "error" }) {
  return (
    <li className="flex flex-col gap-1 rounded-md border p-3">
      <span className="text-xs text-muted-foreground">{title}</span>
      <span
        className={
          tone === "error" ? "text-sm tabular-nums text-status-error" : "text-sm tabular-nums"
        }
      >
        {text}
      </span>
    </li>
  );
}
