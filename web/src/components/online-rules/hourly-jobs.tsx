import { cn } from "cn";
import type { Schema } from "@/api/types";

type RuleJobsHour = Schema<"RuleJobsHour">;

const SEGMENTS: [keyof Omit<RuleJobsHour, "time">, string][] = [
  ["done", "bg-status-ok"],
  ["failed", "bg-status-error"],
  ["skipped", "bg-muted-foreground/30"],
];

/**
 * The finished jobs of each hour of the last 24, as one column per hour. A column's height is its
 * total against the busiest hour; its done, failed and skipped segments stack to that total.
 * Defaults to 96 by 16 px; `className` resizes it.
 */
export function HourlyJobs({ jobs, className }: { jobs: RuleJobsHour[]; className?: string }) {
  const max = Math.max(0, ...jobs.map((hour) => hour.done + hour.failed + hour.skipped));
  return (
    <div
      role="img"
      aria-label="Finished jobs per hour, last 24 h"
      className={cn("flex h-4 w-24 items-end gap-px", className)}
    >
      {jobs.map((hour) => {
        const total = hour.done + hour.failed + hour.skipped;
        return (
          <div key={hour.time} className="flex h-full min-w-0 flex-1 flex-col justify-end">
            {total === 0 ? null : (
              <div className="flex flex-col-reverse" style={{ height: `${(total / max) * 100}%` }}>
                {SEGMENTS.filter(([status]) => hour[status] > 0).map(([status, color]) => (
                  <div
                    key={status}
                    data-status={status}
                    className={color}
                    style={{ height: `${(hour[status] / total) * 100}%` }}
                  />
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
