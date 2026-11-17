import type { Schema } from "@/api/types";
import { PALETTE } from "@/components/charts/metric-chart";

/** Categorical labels shown before the rest is folded into "other". */
const MAX_LABELS = 4;
/** The bars of a numeric histogram are this tall. */
const BAR_HEIGHT = 32;

/** A small chart of one score's distribution, by its type. Text has none. */
export function ScoreDistribution({ summary }: { summary: Schema<"ScoreSummary"> }) {
  switch (summary.data_type) {
    case "numeric":
      return <NumericHistogram summary={summary} />;
    case "boolean":
      return <PassFailBar summary={summary} />;
    case "categorical":
      return <LabelBar summary={summary} />;
    case "text":
      return null;
  }
}

/** The bucket bars, proportional to the fullest bucket; a tooltip gives the bucket's range. */
function NumericHistogram({ summary }: { summary: Schema<"ScoreSummary"> }) {
  const buckets = summary.histogram;
  if (buckets.length === 0) {
    return null;
  }
  const fullest = Math.max(...buckets, 1);
  return (
    <div
      role="img"
      aria-label={`Distribution of ${summary.name}`}
      className="flex items-end gap-px"
      style={{ height: BAR_HEIGHT }}
    >
      {buckets.map((count, index) => (
        <div
          key={bucketRange(summary, index, buckets.length)}
          title={`${bucketRange(summary, index, buckets.length)}: ${count}`}
          className="min-h-px flex-1 rounded-t-sm"
          style={{
            height: `${Math.max((count / fullest) * 100, count > 0 ? 4 : 0)}%`,
            backgroundColor: PALETTE[0],
          }}
        />
      ))}
    </div>
  );
}

/** "min–max" of one bucket; the one-bucket histogram spans the whole range. */
function bucketRange(summary: Schema<"ScoreSummary">, index: number, buckets: number): string {
  if (summary.min === undefined || summary.max === undefined) {
    return `bucket ${index + 1}`;
  }
  if (buckets === 1 || summary.min === summary.max) {
    return `${fixed3(summary.min)}–${fixed3(summary.max)}`;
  }
  const width = (summary.max - summary.min) / 10;
  const low = summary.min + width * index;
  const high = index === 9 ? summary.max : summary.min + width * (index + 1);
  return `${fixed3(low)}–${fixed3(high)}`;
}

function fixed3(value: number): string {
  return value.toFixed(3);
}

/** The pass share in green and the fail share in red. */
function PassFailBar({ summary }: { summary: Schema<"ScoreSummary"> }) {
  if (summary.pass_rate === undefined) {
    return null;
  }
  const passPercent = summary.pass_rate * 100;
  const failPercent = 100 - passPercent;
  const passText = `${Math.round(passPercent * 10) / 10} %`;
  const failText = `${Math.round(failPercent * 10) / 10} %`;
  return (
    <div className="flex flex-col gap-1">
      <div
        role="img"
        aria-label={`Pass ${passText}, fail ${failText}`}
        className="flex h-2 w-full overflow-hidden rounded-sm"
      >
        <div className="h-full bg-status-ok" style={{ width: `${passPercent}%` }} />
        <div className="h-full flex-1 bg-status-error" />
      </div>
      <span className="text-caption text-muted-foreground tabular-nums">
        {`pass ${passText} · fail ${failText}`}
      </span>
    </div>
  );
}

/** The top labels as a stacked bar, the rest as "other", with a legend. */
function LabelBar({ summary }: { summary: Schema<"ScoreSummary"> }) {
  const entries = Object.entries(summary.labels).sort(
    ([nameA, countA], [nameB, countB]) => countB - countA || nameA.localeCompare(nameB),
  );
  const total = entries.reduce((sum, [, count]) => sum + count, 0);
  if (total === 0) {
    return null;
  }
  const top = entries.slice(0, MAX_LABELS);
  const rest = entries.slice(MAX_LABELS).reduce((sum, [, count]) => sum + count, 0);
  const parts: [string, number][] = rest > 0 ? [...top, ["other", rest]] : top;
  return (
    <div className="flex flex-col gap-1">
      <div
        role="img"
        aria-label={`Labels of ${summary.name}`}
        className="flex h-2 w-full overflow-hidden rounded-sm"
      >
        {parts.map(([label, count], index) => (
          <div
            key={label}
            title={`${label}: ${count}`}
            className="h-full"
            style={{
              width: `${(count / total) * 100}%`,
              backgroundColor: label === "other" ? "var(--muted)" : PALETTE[index % PALETTE.length],
            }}
          />
        ))}
      </div>
      <ul className="flex flex-wrap gap-x-3 gap-y-0.5 text-caption text-muted-foreground">
        {parts.map(([label, count], index) => (
          <li key={label} className="flex items-center gap-1">
            <span
              aria-hidden
              className="inline-block size-2 rounded-sm"
              style={{
                backgroundColor:
                  label === "other" ? "var(--muted)" : PALETTE[index % PALETTE.length],
              }}
            />
            {`${label} ${count}`}
          </li>
        ))}
      </ul>
    </div>
  );
}
