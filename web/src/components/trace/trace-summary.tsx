import { useQuery } from "@tanstack/react-query";
import { Coins, DollarSign, ListTree, Sparkles, Timer, TriangleAlert } from "lucide-react";
import { problemOf } from "@/api/problem";
import { spanDetailQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { PayloadView } from "@/components/payload/payload-view";
import { ScoreBadge } from "@/components/score-badge";
import { useProject } from "@/components/shell/project-context";
import { MetricChip } from "@/components/trace/metric-chip";
import { badgeValue } from "@/components/trace/trace-header";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";

/** What the right panel shows in Transcript mode until a span is picked: totals, scores, the question and answer, and errors. */
export function TraceSummary({
  trace,
  spans,
  onSelectSpan,
}: {
  trace: Schema<"TraceSummary">;
  spans: readonly Schema<"TreeSpan">[];
  onSelectSpan: (spanId: string) => void;
}) {
  const { org, project } = useProject();
  // The root is the span with no parent, as the trace view finds it for the header actions.
  const rootSpanId = spans.find((row) => row.span.parent_span_id === undefined)?.span.span_id;
  const root = useQuery({
    ...spanDetailQuery(org, project, trace.trace_id, rootSpanId ?? ""),
    enabled: rootSpanId !== undefined,
  });
  const errors = spans.filter((row) => row.span.status === "error");
  const rootPending = rootSpanId !== undefined && root.isPending;

  return (
    <section
      aria-label="Trace summary"
      className="flex min-h-0 flex-1 flex-col gap-4 overflow-auto px-4 py-3"
    >
      <h2 className="text-title font-semibold">Trace summary</h2>

      <div className="flex flex-wrap gap-1.5">
        <MetricChip icon={Timer} label="Duration">
          {formatDuration(trace.duration_ns)}
        </MetricChip>
        <MetricChip icon={Coins} label="Tokens">
          {formatTokens(trace.total_tokens)}
        </MetricChip>
        <MetricChip icon={DollarSign} label="Cost">
          {formatCost(trace.cost_total)}
        </MetricChip>
        <MetricChip icon={ListTree} label="Spans">
          {`${trace.span_count} spans`}
        </MetricChip>
        <MetricChip icon={Sparkles} label="LLM calls">
          {`${trace.llm_count} LLM calls`}
        </MetricChip>
        <MetricChip icon={TriangleAlert} label="Errors">
          {errors.length === 1 ? "1 error" : `${errors.length} errors`}
        </MetricChip>
      </div>

      <section aria-label="Scores" className="flex flex-col gap-2">
        <h3 className="text-body font-medium">Scores</h3>
        {trace.scores.length === 0 ? (
          <p className="text-body text-muted-foreground">No scores</p>
        ) : (
          <div className="flex flex-wrap gap-1.5">
            {trace.scores.map((score) => (
              <span
                key={score.score_id}
                className="inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-body"
              >
                {score.name}{" "}
                <ScoreBadge name={score.name} value={score.value} text={badgeValue(score.value)} />
              </span>
            ))}
          </div>
        )}
      </section>

      {rootPending ? (
        <div className="flex flex-col gap-2">
          <Skeleton className="h-4 w-1/3" />
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-4 w-1/3" />
          <Skeleton className="h-16 w-full" />
        </div>
      ) : root.isError ? (
        <p className="text-body text-muted-foreground">
          {problemOf(root.error)?.title ?? "Could not load this span"}
        </p>
      ) : (
        <>
          <PayloadView label="Question" payload={root.data?.input} />
          <PayloadView label="Final answer" payload={root.data?.output} />
        </>
      )}

      {errors.length > 0 ? (
        <section aria-label="Errors" className="flex flex-col gap-2">
          <h3 className="text-body font-medium">Errors</h3>
          <ul className="flex flex-col gap-1">
            {errors.map((row) => (
              <li key={row.span.span_id}>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="h-auto w-full flex-col items-start gap-0.5 px-2 py-1 text-body"
                  onClick={() => {
                    onSelectSpan(row.span.span_id);
                  }}
                >
                  <span className="font-medium">{row.span.name}</span>
                  {row.span.status_message ? (
                    <span className="text-muted-foreground">{row.span.status_message}</span>
                  ) : null}
                </Button>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
    </section>
  );
}
