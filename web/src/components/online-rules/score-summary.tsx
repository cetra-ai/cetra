import type { Schema } from "@/api/types";
import { Skeleton } from "@/components/ui/skeleton";
import { ScoreLine } from "./score-line";

type RuleScores = Schema<"RuleScoreStats">;

/** The value of a rule's scores in the window, as text. Shared by the list cell and the rule page. */
export function scoreText(scores: RuleScores): string {
  if (scores.count === 0) {
    return "No scores";
  }
  switch (scores.data_type) {
    case "numeric":
      return scores.average === undefined ? "—" : scores.average.toFixed(2);
    case "boolean":
      return scores.pass_rate === undefined ? "—" : `${Math.round(scores.pass_rate * 100)} %`;
    case "categorical":
      return scores.top_category === undefined
        ? "—"
        : `${scores.top_category.label} (${scores.top_category.count})`;
    case "text":
      return `${scores.count} scores`;
  }
}

/** The number a rule sorts by: the average (numeric) or the pass rate (boolean). Others have none. */
export function scoreValue(scores: RuleScores | undefined): number | undefined {
  if (scores === undefined) {
    return undefined;
  }
  if (scores.data_type === "numeric") {
    return scores.average;
  }
  if (scores.data_type === "boolean") {
    return scores.pass_rate;
  }
  return undefined;
}

/** "3 scores · 24 h" under the value. */
export function scoreCaption(scores: RuleScores): string {
  return `${scores.count} ${scores.count === 1 ? "score" : "scores"} · 24 h`;
}

/** Numeric and boolean rules have a per-hour trend line; categorical and text ones do not. */
export function scoreTrendLabel(scores: RuleScores): string | undefined {
  switch (scores.data_type) {
    case "numeric":
      return "Average score per hour, last 24 h";
    case "boolean":
      return "Pass rate per hour, last 24 h";
    case "categorical":
    case "text":
      return undefined;
  }
}

/** The per-hour values of the trend line; `null` for an hour without a value. */
export function scoreTrendValues(scores: RuleScores): (number | null)[] {
  return scores.hourly.map((point) => point.value ?? null);
}

/** A trend value as the summary shows it: a percentage for pass rates. */
function trendFormat(scores: RuleScores): (value: number) => string {
  return scores.data_type === "boolean"
    ? (value) => `${Math.round(value * 100)} %`
    : (value) => value.toFixed(2);
}

/** A rule's scores on its page: the value large, the count, and a full-width trend line. */
export function ScoreTrend({ scores }: { scores: RuleScores }) {
  const label = scoreTrendLabel(scores);
  return (
    <div className="flex flex-col gap-2">
      <p className="text-title tabular-nums">{scoreText(scores)}</p>
      {scores.count === 0 ? null : (
        <p className="text-caption text-muted-foreground">{scoreCaption(scores)}</p>
      )}
      {label === undefined || scores.count === 0 ? null : (
        <ScoreLine
          values={scoreTrendValues(scores)}
          boolean={scores.data_type === "boolean"}
          label={label}
          axis
          format={trendFormat(scores)}
          className="h-20 w-full"
        />
      )}
    </div>
  );
}

/** A rule's score summary in the list: the value, a 64 by 16 trend line and the count. */
export function ScoreSummary({
  scores,
  pending,
}: {
  scores: RuleScores | undefined;
  pending: boolean;
}) {
  if (pending) {
    return <Skeleton className="h-4 w-16" />;
  }
  if (scores === undefined) {
    return <span className="text-body text-muted-foreground">—</span>;
  }
  const label = scoreTrendLabel(scores);
  const muted = scores.count === 0;
  return (
    <div className="flex flex-col items-start gap-1">
      <div className="flex items-center gap-2 whitespace-nowrap">
        <span className={muted ? "text-body text-muted-foreground" : "text-body tabular-nums"}>
          {scoreText(scores)}
        </span>
        {label !== undefined && !muted ? (
          <ScoreLine
            values={scoreTrendValues(scores)}
            boolean={scores.data_type === "boolean"}
            label={label}
          />
        ) : null}
      </div>
      {muted ? null : (
        <span className="text-caption text-muted-foreground">{scoreCaption(scores)}</span>
      )}
    </div>
  );
}
