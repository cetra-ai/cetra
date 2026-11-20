import type { Schema } from "@/api/types";
import { progressOf } from "@/lib/annotation-queues";

/** "n of N done" over a thin bar, from the queue's item counts. */
export function QueueProgress({ counts }: { counts: Schema<"QueueCounts"> }) {
  const { done, total } = progressOf(counts);
  const fill = total === 0 ? 0 : Math.round((done / total) * 100);
  return (
    <div className="flex min-w-32 flex-col gap-1">
      <span className="text-body tabular-nums">{`${done} of ${total} done`}</span>
      <div
        role="progressbar"
        aria-label="Reviewed"
        aria-valuemin={0}
        aria-valuemax={total}
        aria-valuenow={done}
        className="h-1 overflow-hidden rounded-full bg-muted"
      >
        <div className="h-full rounded-full bg-primary" style={{ width: `${fill}%` }} />
      </div>
    </div>
  );
}
