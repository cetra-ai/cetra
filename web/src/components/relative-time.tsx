import { formatRelative, formatTimestamp } from "@/lib/format";
import { useTimeZone } from "@/lib/settings";

/** Relative time; the exact time in the user's zone is the tooltip. */
export function RelativeTime({ value }: { value: string }) {
  const zone = useTimeZone();
  return (
    <time
      dateTime={value}
      title={formatTimestamp(value, zone, "datetime")}
      className="tabular-nums"
    >
      {formatRelative(value, Date.now())}
    </time>
  );
}
