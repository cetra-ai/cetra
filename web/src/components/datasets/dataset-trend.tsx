import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { createContext, type ReactNode, useContext } from "react";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";

export type ExperimentTrend = Schema<"ExperimentTrend">;
type ScoreMean = Schema<"ExperimentScoreMean">;
type MeasuredScore = ScoreMean & { mean: number };

/** A dataset's answer in the trends: its latest experiments, or none when the request failed. */
export type DatasetTrendState = { experiments: readonly ExperimentTrend[]; unavailable: boolean };

export type TrendPoint = { id: string; value: number; title: string };

/** The score a dataset shows: the latest experiment's score, and the same score over the experiments. */
export type ScoreTrend = {
  name: string;
  dataType: string;
  latest: string;
  points: TrendPoint[];
};

export const UNAVAILABLE_TEXT = "Scores are unavailable right now";

/**
 * The trends of the loaded datasets, by id. The list provides it to its cells: a memoised table row
 * does not re-render when only its columns' data changes, but a context consumer does.
 */
export const DatasetTrendsContext = createContext<ReadonlyMap<string, DatasetTrendState>>(
  new Map(),
);

/** The trend of one dataset from the nearest provider, undefined while it is still loading. */
export function useDatasetTrend(datasetId: string): DatasetTrendState | undefined {
  return useContext(DatasetTrendsContext).get(datasetId);
}

function hasMean(score: ScoreMean): score is MeasuredScore {
  return score.mean !== undefined;
}

/** The text of a score's mean: two decimals for numeric scores, a percentage for booleans (their pass rate). */
export function formatScore(dataType: string, value: number): string {
  return dataType === "boolean" ? `${Math.round(value * 100)}%` : value.toFixed(2);
}

/**
 * The score of a dataset's latest experiment: its first numeric mean, else its first boolean mean.
 * `points` are the same score over the experiments, oldest first. Undefined when the latest
 * experiment has no such score.
 */
export function scoreTrend(experiments: readonly ExperimentTrend[]): ScoreTrend | undefined {
  const latest = experiments[0];
  if (!latest) {
    return undefined;
  }
  const measured = (latest.scores ?? []).filter(hasMean);
  const chosen =
    measured.find((score) => score.data_type === "numeric") ??
    measured.find((score) => score.data_type === "boolean");
  if (!chosen) {
    return undefined;
  }
  const points: TrendPoint[] = [];
  for (const experiment of [...experiments].reverse()) {
    const match = (experiment.scores ?? []).find(
      (score) => score.name === chosen.name && score.data_type === chosen.data_type,
    );
    if (match?.mean === undefined) {
      continue;
    }
    const text = formatScore(chosen.data_type, match.mean);
    points.push({ id: experiment.id, value: match.mean, title: `${experiment.name}: ${text}` });
  }
  return {
    name: chosen.name,
    dataType: chosen.data_type,
    latest: formatScore(chosen.data_type, chosen.mean),
    points,
  };
}

/** The error rate of each experiment with results, oldest first. */
export function errorRatePoints(experiments: readonly ExperimentTrend[]): TrendPoint[] {
  const points: TrendPoint[] = [];
  for (const experiment of [...experiments].reverse()) {
    if (experiment.result_count > 0) {
      const rate = experiment.error_count / experiment.result_count;
      points.push({
        id: experiment.id,
        value: rate,
        title: `${experiment.name}: ${(rate * 100).toFixed(1)}% errors`,
      });
    }
  }
  return points;
}

/**
 * The trends of each loaded dataset, from one answer per list page. A page still loading leaves its
 * rows out (they show a skeleton). A page that failed, or a dataset the answer leaves out, counts
 * as no experiments, so no row waits forever.
 */
export function collectTrends(
  pages: readonly {
    ids: readonly string[];
    answer: { data?: Schema<"ExperimentTrends">; isError: boolean };
  }[],
): Map<string, DatasetTrendState> {
  const trends = new Map<string, DatasetTrendState>();
  for (const { ids, answer } of pages) {
    if (answer.data === undefined && !answer.isError) {
      continue;
    }
    const byId = new Map((answer.data?.data ?? []).map((trend) => [trend.dataset_id, trend]));
    for (const id of ids) {
      trends.set(id, {
        experiments: byId.get(id)?.experiments ?? [],
        unavailable: answer.data?.scores_unavailable ?? false,
      });
    }
  }
  return trends;
}

/** A line over the points, one dot each. Dots are wrapped by `renderPoint` (for links) when given. */
export function TrendLine({
  points,
  label,
  domain,
  className,
  renderPoint,
}: {
  points: readonly TrendPoint[];
  label: string;
  domain?: readonly [number, number];
  className?: string;
  renderPoint?: (point: TrendPoint, dot: ReactNode) => ReactNode;
}) {
  if (points.length === 0) {
    return null;
  }
  const width = 100;
  const height = 24;
  const pad = 4;
  const values = points.map((point) => point.value);
  const low = domain?.[0] ?? Math.min(...values);
  const high = domain?.[1] ?? Math.max(...values);
  const span = high - low;
  const x = (index: number) =>
    points.length === 1 ? width / 2 : pad + (index * (width - 2 * pad)) / (points.length - 1);
  const y = (value: number) =>
    span > 0 ? height - pad - ((value - low) / span) * (height - 2 * pad) : height / 2;
  const line = points.map((point, index) => `${x(index)},${y(point.value)}`).join(" ");
  return (
    <svg
      role="img"
      aria-label={label}
      viewBox={`0 0 ${width} ${height}`}
      className={cn("h-4 w-24 overflow-visible text-primary", className)}
    >
      {points.length > 1 ? (
        <polyline
          points={line}
          fill="none"
          stroke="currentColor"
          strokeWidth={1.5}
          vectorEffect="non-scaling-stroke"
        />
      ) : null}
      {points.map((point, index) => {
        const dot = (
          <circle cx={x(index)} cy={y(point.value)} r={2.5} fill="currentColor">
            <title>{point.title}</title>
          </circle>
        );
        return <g key={point.id}>{renderPoint ? renderPoint(point, dot) : dot}</g>;
      })}
    </svg>
  );
}

/** The dash shown where a score cannot be shown, with the reason when storage was unavailable. */
export function UnavailableScore({ unavailable }: { unavailable: boolean }) {
  if (!unavailable) {
    return <span className="text-muted-foreground">—</span>;
  }
  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <span className="text-muted-foreground">—</span>
        </TooltipTrigger>
        <TooltipContent>{UNAVAILABLE_TEXT}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}

/** The score trend and error rate of a dataset's experiments, with each point linking to its experiment. */
export function ExperimentTrendChart({
  experiments,
  unavailable,
}: {
  experiments: readonly ExperimentTrend[];
  unavailable: boolean;
}) {
  const { org, project } = useProject();
  if (unavailable) {
    return <p className="text-muted-foreground">{UNAVAILABLE_TEXT}</p>;
  }
  if (experiments.length === 0) {
    return <p className="text-muted-foreground">—</p>;
  }
  const score = scoreTrend(experiments);
  const errors = errorRatePoints(experiments);
  const renderPoint = (point: TrendPoint, dot: ReactNode) => (
    <Link
      to="/$org/$project/experiments/$experimentId"
      params={{ org, project, experimentId: point.id }}
      aria-label={point.title}
    >
      {dot}
    </Link>
  );
  return (
    <div className="flex flex-col gap-3">
      {score ? (
        <div className="flex flex-col gap-1">
          <span className="flex items-baseline gap-2">
            <span className="min-w-0 truncate text-muted-foreground">{score.name}</span>
            <span className="num">{score.latest}</span>
          </span>
          <TrendLine
            points={score.points}
            label={`${score.name} over the last experiments`}
            className="h-8 w-full"
            renderPoint={renderPoint}
          />
        </div>
      ) : (
        <p className="text-muted-foreground">No numeric or boolean score yet</p>
      )}
      {errors.length > 0 ? (
        <div className="flex flex-col gap-1">
          <span className="flex items-baseline gap-2">
            <span className="text-muted-foreground">Error rate</span>
            <span className="num">
              {((errors[errors.length - 1]?.value ?? 0) * 100).toFixed(1)}%
            </span>
          </span>
          <TrendLine
            points={errors}
            label="Error rate over the last experiments"
            domain={[0, 1]}
            className="h-8 w-full"
            renderPoint={renderPoint}
          />
        </div>
      ) : null}
    </div>
  );
}
