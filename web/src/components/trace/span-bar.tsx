import { cn } from "cn";
import type { Schema } from "@/api/types";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatCost, formatDuration, formatTimestamp, instantNs } from "@/lib/format";
import { useTimeZone } from "@/lib/settings";
import { type BarGeometry, barClipPath, labelPlacement, type SlowLevel } from "@/lib/timeline";

// The labels are 10px mono text, about 6px a character, with a small gap to the bar.
const LABEL_CHAR_PX = 6;
const LABEL_GAP_PX = 4;

/** A bar's label: its duration, then its cost when it has one. */
export function barLabel(span: { duration_ns: number; cost_total?: number }): string {
  const duration = formatDuration(span.duration_ns);
  return span.cost_total === undefined ? duration : `${duration}  ${formatCost(span.cost_total)}`;
}

/**
 * A span's bar on the timeline, outlined amber or red when slow, with its duration and cost
 * written inside it when they fit, else beside it (right, or left near the trace's end), and a
 * tooltip with the start, end and total time.
 */
export function SpanBar({
  span,
  bar,
  colorClass,
  timelineWidth,
  traceStartNs,
  slow,
  onDoubleClick,
}: {
  span: Schema<"TreeSpan">["span"];
  bar: BarGeometry;
  colorClass: string;
  timelineWidth: number;
  traceStartNs: number;
  slow?: SlowLevel;
  onDoubleClick: () => void;
}) {
  const zone = useTimeZone();
  const duration = formatDuration(span.duration_ns);
  const label = barLabel(span);
  const placement = labelPlacement(
    bar,
    label.length * LABEL_CHAR_PX + LABEL_GAP_PX * 2,
    timelineWidth,
  );
  const endPct = bar.leftPct + bar.widthPct;
  const labelStyle =
    placement === "inside"
      ? { left: `calc(${bar.leftPct}% + ${LABEL_GAP_PX}px)` }
      : placement === "right"
        ? { left: `calc(${endPct}% + ${LABEL_GAP_PX}px)` }
        : { right: `calc(${100 - bar.leftPct}% + ${LABEL_GAP_PX}px)` };
  const startNs = instantNs(span.start_time);
  const endNs = instantNs(span.end_time);

  return (
    <>
      <Tooltip>
        <TooltipTrigger asChild>
          <button
            type="button"
            tabIndex={-1}
            aria-label={`Timeline ${span.name}`}
            data-slow={slow}
            className={cn(
              "absolute top-1/2 h-[18px] -translate-y-1/2 rounded-[4px] border-0 p-0",
              colorClass,
              slow
                ? cn(
                    "outline-2 outline-offset-1 outline-solid",
                    slow === "critical" ? "outline-status-error" : "outline-status-warn",
                  )
                : span.status === "error" && "outline outline-1 outline-status-error",
            )}
            style={{
              left: `${bar.leftPct}%`,
              width: `${bar.widthPct}%`,
              clipPath: barClipPath(bar),
            }}
            onDoubleClick={(event) => {
              event.stopPropagation();
              onDoubleClick();
            }}
          />
        </TooltipTrigger>
        <TooltipContent className="flex-col items-start gap-0.5">
          <span className="font-medium">{span.name}</span>
          <span className="tabular-nums">
            Start {formatTimestamp(span.start_time, zone, "time")} (+
            {formatDuration(startNs - traceStartNs)})
          </span>
          <span className="tabular-nums">
            End {formatTimestamp(span.end_time, zone, "time")} (+
            {formatDuration(endNs - traceStartNs)})
          </span>
          <span className="tabular-nums">Duration {duration}</span>
        </TooltipContent>
      </Tooltip>
      <span
        aria-hidden="true"
        data-placement={placement}
        className={cn(
          "pointer-events-none absolute top-1/2 -translate-y-1/2 font-mono text-caption whitespace-pre tabular-nums",
          placement === "inside"
            ? "font-medium text-background"
            : slow === "critical"
              ? "text-status-error"
              : slow === "warn"
                ? "text-status-warn"
                : "text-muted-foreground",
        )}
        style={labelStyle}
      >
        {label}
      </span>
    </>
  );
}
