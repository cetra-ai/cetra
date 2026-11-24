import { cn } from "cn";
import type { Schema } from "@/api/types";
import { formatDuration } from "@/lib/format";
import { stripWeight } from "@/lib/session-turns";

/**
 * One cell per turn, sized by its duration and coloured by its status (D-C-28). A click selects
 * the turn. The cells are not tab stops: the conversation's j and k keys are the keyboard path.
 */
export function TurnStrip({
  turns,
  visible,
  selectedTraceId,
  onPick,
}: {
  turns: readonly Schema<"SessionTurn">[];
  /** Trace ids of the turns the search and filters keep. */
  visible: ReadonlySet<string>;
  selectedTraceId: string | undefined;
  onPick: (traceId: string) => void;
}) {
  if (turns.length < 2) {
    return null;
  }
  return (
    <div role="group" aria-label="Turns overview" className="flex h-4 gap-px px-4 py-1">
      {turns.map((turn, offset) => {
        const traceId = turn.trace.trace_id;
        const failed = turn.trace.status === "error";
        const label = `#${offset + 1} ${turn.trace.name}, ${formatDuration(turn.trace.duration_ns)}${failed ? ", failed" : ""}`;
        return (
          <button
            key={traceId}
            type="button"
            tabIndex={-1}
            aria-label={label}
            title={label}
            className={cn(
              "focus-ring min-w-[3px] rounded-[1px]",
              failed ? "bg-status-error" : "bg-muted-foreground/30",
              traceId === selectedTraceId && "ring-1 ring-primary",
              !visible.has(traceId) && "opacity-30",
            )}
            style={{ flexGrow: stripWeight(turn), flexBasis: 0 }}
            onClick={() => {
              onPick(traceId);
            }}
          />
        );
      })}
    </div>
  );
}
