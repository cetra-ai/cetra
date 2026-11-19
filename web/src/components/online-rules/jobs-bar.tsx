import type { Schema } from "@/api/types";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

type JobCounts = Schema<"JobCounts">;

const SEGMENTS: [keyof JobCounts, string, string][] = [
  ["done", "done", "bg-status-ok"],
  ["failed", "failed", "bg-status-error"],
  ["running", "running", "bg-status-info"],
  ["queued", "queued", "bg-status-unset"],
  ["skipped", "skipped", "bg-muted-foreground/30"],
];

/** "40 done", "3 failed", for the statuses that have jobs. */
function countLines(counts: JobCounts): string[] {
  return SEGMENTS.filter(([status]) => counts[status] > 0).map(
    ([status, label]) => `${counts[status]} ${label}`,
  );
}

/** A stacked bar of the 24-hour job statuses, the total and the failures. The caller provides TooltipProvider. */
export function JobsBar({ counts }: { counts: JobCounts }) {
  const total = SEGMENTS.reduce((sum, [status]) => sum + counts[status], 0);
  if (total === 0) {
    return <span className="text-body text-muted-foreground">No jobs</span>;
  }
  const lines = countLines(counts);
  const failed = counts.failed;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <div className="flex items-center gap-2 text-body tabular-nums">
          <div
            role="img"
            aria-label={lines.join(", ")}
            className="flex h-1.5 w-24 shrink-0 overflow-hidden rounded-full"
          >
            {SEGMENTS.filter(([status]) => counts[status] > 0).map(([status, , color]) => (
              <div
                key={status}
                data-status={status}
                className={color}
                style={{ width: `${(counts[status] / total) * 100}%` }}
              />
            ))}
          </div>
          <span>
            {total}
            {failed > 0 ? (
              <>
                {" · "}
                <span className="text-status-error">{failed} failed</span>
              </>
            ) : null}
          </span>
        </div>
      </TooltipTrigger>
      <TooltipContent className="flex-col items-start">
        {lines.map((line) => (
          <span key={line}>{line}</span>
        ))}
      </TooltipContent>
    </Tooltip>
  );
}
