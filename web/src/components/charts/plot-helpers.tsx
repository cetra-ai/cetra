import type { BarShapeProps } from "recharts";
import { timeTicks } from "@/lib/chart-scale";
import type { ChartRow } from "@/lib/metrics";
import type { TimeZoneSetting } from "@/lib/settings";

export const DAY_MS = 24 * 60 * 60 * 1000;
// Faded bars and buckets are drawn at this opacity: their value is noisy.
const THIN_OPACITY = 0.35;

export const AXIS_TICK = {
  fontSize: 11,
  fontFamily: "var(--font-mono)",
  fill: "var(--muted-foreground)",
} as const;

/**
 * The X-axis tick times: the row times that fall on round times of the shown zone (see
 * `timeTicks`). When none does (for example one-day buckets in a local zone), every k-th row
 * with k = ceil(rows / maxTicks), so the axis has at most about `maxTicks` labels.
 */
export function xAxisTicks(rows: readonly ChartRow[], utc: boolean, maxTicks = 8): number[] {
  const times = rows.map((row) => row.time);
  const first = times[0];
  const last = times.at(-1);
  if (first === undefined || last === undefined) {
    return [];
  }
  const round = new Set(timeTicks(first, last, utc, maxTicks));
  const onRound = times.filter((time) => round.has(time));
  if (onRound.length > 0) {
    return onRound;
  }
  const every = Math.ceil(times.length / maxTicks);
  return times.filter((_, index) => index % every === 0);
}

/** Whether exactly one row has a value under `key`: a lone point then gets a marker. */
export function solitary(rows: readonly ChartRow[], key: string): boolean {
  let count = 0;
  for (const row of rows) {
    const value = row[key];
    if (value !== null && value !== undefined) {
      count += 1;
      if (count > 1) {
        return false;
      }
    }
  }
  return count === 1;
}

export function bucketLabel(timeMs: number): string {
  return `Open bucket ${new Date(timeMs).toISOString()}`;
}

export function axisStyle(rows: readonly { time: number }[]): "time" | "datetime" {
  const first = rows[0]?.time;
  const last = rows[rows.length - 1]?.time;
  if (first === undefined || last === undefined || last - first < DAY_MS) {
    return "time";
  }
  return "datetime";
}

export function formatTick(
  value: unknown,
  zone: TimeZoneSetting,
  style: "time" | "datetime",
): string {
  const ms = typeof value === "number" ? value : Number(value);
  if (Number.isNaN(ms)) {
    return "";
  }
  const date = new Date(ms);
  const utc = zone === "utc";
  const hours = pad(utc ? date.getUTCHours() : date.getHours());
  const minutes = pad(utc ? date.getUTCMinutes() : date.getMinutes());
  const clock = `${hours}:${minutes}`;
  if (style === "time") {
    return clock;
  }
  const month = pad((utc ? date.getUTCMonth() : date.getMonth()) + 1);
  const day = pad(utc ? date.getUTCDate() : date.getDate());
  return `${month}-${day} ${clock}`;
}

function pad(value: number): string {
  return String(value).padStart(2, "0");
}

/**
 * The marker of a line point, with a bucket button over it when `onBucket` is given. The button
 * is mouse-only: the chart group is the keyboard path (D-C-3).
 */
export function lineDot(
  dataKey: string,
  onBucket: ((timeMs: number) => void) | undefined,
  showMarker: boolean,
) {
  return (props: unknown) => {
    if (typeof props !== "object" || props === null) {
      return <g />;
    }
    const point = props as {
      cx?: number;
      cy?: number;
      payload?: ChartRow;
      value?: number | null;
      stroke?: string;
    };
    const time = point.payload?.time;
    const value = point.value ?? (point.payload ? point.payload[dataKey] : undefined);
    if (
      typeof point.cx !== "number" ||
      typeof point.cy !== "number" ||
      typeof time !== "number" ||
      value === null ||
      value === undefined
    ) {
      return <g />;
    }
    return (
      <g>
        {showMarker ? <circle cx={point.cx} cy={point.cy} r={3} fill={point.stroke} /> : null}
        {onBucket ? (
          // biome-ignore lint/a11y/noAriaHiddenOnFocusable: mouse-only bucket target with tabIndex -1; the chart group is the keyboard path (D-C-3)
          <foreignObject
            x={point.cx - 8}
            y={point.cy - 8}
            width={16}
            height={16}
            aria-hidden="true"
          >
            <button
              type="button"
              tabIndex={-1}
              aria-label={bucketLabel(time)}
              className="focus-ring block size-full cursor-pointer border-0 bg-transparent p-0"
              onClick={() => onBucket(time)}
            />
          </foreignObject>
        ) : null}
      </g>
    );
  };
}

/**
 * A bar of one bucket. It is faded when the row's `fadeKey` value is below `fadeBelow`, and it
 * is a bucket button when `onBucket` is given (mouse only, as `lineDot`).
 */
export function BucketBar({
  x,
  y,
  width,
  height,
  fill,
  payload,
  onBucket,
  fadeKey,
  fadeBelow,
}: BarShapeProps & {
  onBucket?: (timeMs: number) => void;
  fadeKey?: string;
  fadeBelow?: number;
}) {
  const left = x ?? 0;
  const top = y ?? 0;
  const time = rowTime(payload);
  const count = fadeKey === undefined ? undefined : rowNumber(payload, fadeKey);
  const fillOpacity =
    count !== undefined && fadeBelow !== undefined && count < fadeBelow ? THIN_OPACITY : undefined;
  if (time === undefined || width <= 0 || height <= 0 || !onBucket) {
    return (
      <rect
        x={left}
        y={top}
        width={Math.max(width, 0)}
        height={Math.max(height, 0)}
        fill={fill}
        fillOpacity={fillOpacity}
      />
    );
  }
  return (
    <g>
      <rect x={left} y={top} width={width} height={height} fill={fill} fillOpacity={fillOpacity} />
      {/* biome-ignore lint/a11y/noAriaHiddenOnFocusable: mouse-only bucket target with tabIndex -1; the chart group is the keyboard path (D-C-3) */}
      <foreignObject x={left} y={top} width={width} height={height} aria-hidden="true">
        <button
          type="button"
          tabIndex={-1}
          aria-label={bucketLabel(time)}
          className="focus-ring block h-full w-full cursor-pointer border-0 bg-transparent p-0"
          onClick={() => onBucket(time)}
        />
      </foreignObject>
    </g>
  );
}

function rowTime(payload: unknown): number | undefined {
  return rowNumber(payload, "time");
}

function rowNumber(payload: unknown, key: string): number | undefined {
  if (typeof payload !== "object" || payload === null || !(key in payload)) {
    return undefined;
  }
  const value = (payload as Record<string, unknown>)[key];
  return typeof value === "number" ? value : undefined;
}

/** The largest value to fit: the sum of the stacked columns in a row, or the largest one. */
export function largestValue(
  columns: readonly { key: string; stacked: boolean }[],
  rows: readonly ChartRow[],
): number {
  let max = 0;
  for (const row of rows) {
    let stackSum = 0;
    let peak = 0;
    let any = false;
    for (const column of columns) {
      const value = row[column.key];
      if (typeof value !== "number" || Number.isNaN(value)) {
        continue;
      }
      any = true;
      if (column.stacked) {
        stackSum += value;
      }
      peak = Math.max(peak, value);
    }
    if (any) {
      max = Math.max(max, stackSum, peak);
    }
  }
  return max;
}
