import { formatTick } from "@/components/charts/plot-helpers";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { formatRelative, formatTimestamp } from "@/lib/format";
import { type TimeZoneSetting, useTimeZone } from "@/lib/settings";

/** `time` and `datetime` keep the milliseconds; `minute` (11:51) and `date-minute` (11-19 11:51)
 *  are the chart axis formats, for times that are whole minutes or need no more precision. */
export type TimeTextStyle = "time" | "datetime" | "minute" | "date-minute";

export function TimeText({ value, style }: { value: string; style: TimeTextStyle }) {
  const zone = useTimeZone();
  const other = zone === "utc" ? "local" : "utc";
  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <time dateTime={value} data-zone={zone} className="tabular-nums">
            {formatTimeText(value, zone, style)}
          </time>
        </TooltipTrigger>
        <TooltipContent className="flex-col items-start">
          <span>{value}</span>
          <span>{formatTimestamp(value, other, "datetime")}</span>
          <span>{formatRelative(value, Date.now())}</span>
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}

function formatTimeText(value: string, zone: TimeZoneSetting, style: TimeTextStyle): string {
  if (style === "time" || style === "datetime") {
    return formatTimestamp(value, zone, style);
  }
  const ms = Date.parse(value);
  if (Number.isNaN(ms)) {
    return value;
  }
  return formatTick(ms, zone, style === "minute" ? "time" : "datetime");
}
