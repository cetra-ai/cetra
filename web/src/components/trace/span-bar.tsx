import { cn } from "cn";
import type { Schema } from "@/api/types";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatDuration, formatTimestamp, instantNs } from "@/lib/format";
import { useTimeZone } from "@/lib/settings";
import { type BarGeometry, barClipPath } from "@/lib/timeline";

// The labels are 10px mono text, about 6px a character, with a small gap to the bar.
const LABEL_CHAR_PX = 6;
const LABEL_GAP_PX = 4;

/**
 * A span's bar on the timeline, with its duration written beside it (right of the bar, else left,
 * else inside it) and a tooltip with the start, end and total time.
 */
export function SpanBar({
  span,
  bar,
  colorClass,
  timelineWidth,
  traceStartNs,
  onDoubleClick,
}: {
  span: Schema<"TreeSpan">["span"];
  bar: BarGeometry;
  colorClass: string;
  timelineWidth: number;
  traceStartNs: number;
  onDoubleClick: () => void;
}) {
  const zone = useTimeZone();
  const duration = formatDuration(span.duration_ns);
  const labelPx = duration.length * LABEL_CHAR_PX + LABEL_GAP_PX * 2;
  const endPct = bar.leftPct + bar.widthPct;
  const startPx = (bar.leftPct / 100) * timelineWidth;
  const endPx = (endPct / 100) * timelineWidth;
  const placement =
    endPx + labelPx <= timelineWidth ? "right" : startPx >= labelPx ? "left" : "inside";
  const labelStyle =
    placement === "right"
      ? { left: `calc(${endPct}% + ${LABEL_GAP_PX}px)` }
      : placement === "left"
        ? { right: `calc(${100 - bar.leftPct}% + ${LABEL_GAP_PX}px)` }
        : { right: `calc(${100 - endPct}% + ${LABEL_GAP_PX}px)` };
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
            className={cn(
              "absolute top-1/2 h-3 -translate-y-1/2 rounded-sm border-0 p-0",
              colorClass,
              span.status === "error" && "outline outline-1 outline-status-error",
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
          "pointer-events-none absolute top-1/2 -translate-y-1/2 font-mono text-[10px] whitespace-nowrap text-muted-foreground tabular-nums",
          placement === "inside" && "rounded-sm bg-background/80 px-1 text-foreground",
        )}
        style={labelStyle}
      >
        {duration}
      </span>
    </>
  );
}
