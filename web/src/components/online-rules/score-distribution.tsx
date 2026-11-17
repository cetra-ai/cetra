import { useQuery } from "@tanstack/react-query";
import { scoresQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { Skeleton } from "@/components/ui/skeleton";

type Score = Schema<"Score">;
type Value = Schema<"ScoreValue">;

export interface Distribution {
  kind: "numeric" | "boolean" | "categorical" | "text" | "empty";
  bins: { label: string; count: number }[];
}

const BIN_COUNT = 10;

/** Bins of the scores of one rule. Deleted scores are left out; the first live score's type decides the kind. */
export function distribution(scores: Score[]): Distribution {
  const live = scores.filter((score) => !score.deleted);
  if (live.length === 0) {
    return { kind: "empty", bins: [] };
  }
  const values: Value[] = live.map((score) => score.value);
  const first = values[0];
  if (first === undefined) {
    return { kind: "empty", bins: [] };
  }
  const kind = first.data_type;
  if (kind === "text") {
    return { kind: "text", bins: [] };
  }
  if (kind === "numeric") {
    const numbers = values.flatMap((value) => (value.data_type === "numeric" ? [value.value] : []));
    return { kind: "numeric", bins: numericBins(numbers) };
  }
  if (kind === "boolean") {
    const flags = values.flatMap((value) => (value.data_type === "boolean" ? [value.value] : []));
    return {
      kind: "boolean",
      bins: [
        { label: "true", count: flags.filter((flag) => flag).length },
        { label: "false", count: flags.filter((flag) => !flag).length },
      ],
    };
  }
  const counts = new Map<string, number>();
  for (const value of values) {
    if (value.data_type === "categorical") {
      counts.set(value.value.label, (counts.get(value.value.label) ?? 0) + 1);
    }
  }
  const bins = [...counts]
    .map(([label, count]) => ({ label, count }))
    .sort((a, b) => b.count - a.count || (a.label < b.label ? -1 : a.label > b.label ? 1 : 0));
  return { kind: "categorical", bins };
}

function numericBins(numbers: number[]): { label: string; count: number }[] {
  const min = Math.min(...numbers);
  const max = Math.max(...numbers);
  if (min === max) {
    return [{ label: min.toFixed(2), count: numbers.length }];
  }
  const width = (max - min) / BIN_COUNT;
  const counts = Array.from({ length: BIN_COUNT }, () => 0);
  for (const number of numbers) {
    const index = Math.min(BIN_COUNT - 1, Math.floor((number - min) / width));
    counts[index] = (counts[index] ?? 0) + 1;
  }
  return counts.map((count, index) => ({
    label: `${(min + index * width).toFixed(2)}–${(min + (index + 1) * width).toFixed(2)}`,
    count,
  }));
}

/** The distribution of the scores that match `filters`: one rule's, or one evaluator's. */
export function ScoreDistribution({
  org,
  project,
  filters,
}: {
  org: string;
  project: string;
  filters: { rule_id?: string; evaluator_id?: string };
}) {
  const query = useQuery(scoresQuery(org, project, { ...filters, limit: 1000 }));
  if (query.isPending) {
    return <Skeleton className="h-24 w-full" />;
  }
  if (query.isError) {
    return (
      <p role="alert" className="text-caption text-destructive">
        Could not load the scores.
      </p>
    );
  }
  const scores = query.data.data;
  const result = distribution(scores);
  const latest = scores.filter((score) => !score.deleted).length;
  const largest = Math.max(1, ...result.bins.map((bin) => bin.count));

  return (
    <div className="flex flex-col gap-3">
      <h3 className="text-body font-medium">Score distribution (latest {latest} scores)</h3>
      {result.kind === "empty" ? (
        <p className="text-caption text-muted-foreground">No scores yet.</p>
      ) : result.kind === "text" ? (
        <p className="text-caption text-muted-foreground">Text scores have no distribution.</p>
      ) : (
        result.bins.map((bin) => (
          <div
            key={bin.label}
            className="grid grid-cols-[9rem_1fr_3rem] items-center gap-2 text-caption"
          >
            <span className="truncate font-mono tabular-nums">{bin.label}</span>
            <div className="h-3 rounded-sm bg-muted">
              <div
                role="img"
                aria-label={`${bin.label}: ${bin.count}`}
                className="h-full rounded-sm bg-primary"
                style={{ width: `${(bin.count / largest) * 100}%` }}
              />
            </div>
            <span className="text-right tabular-nums">{bin.count}</span>
          </div>
        ))
      )}
    </div>
  );
}
