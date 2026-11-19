import { cn } from "cn";

/** The y range of a score line: 0 to 1 for pass rates, the values' own range otherwise. */
export function scoreLineDomain(
  values: readonly (number | null)[],
  boolean: boolean,
): [number, number] {
  if (boolean) {
    return [0, 1];
  }
  const present = values.filter((value): value is number => value !== null);
  if (present.length === 0) {
    return [0, 1];
  }
  const min = Math.min(...present);
  const max = Math.max(...present);
  return min === max ? [min - 0.5, max + 0.5] : [min, max];
}

/**
 * Line segments of consecutive hours that have a value; an hour without a value breaks the line.
 * Coordinates are in a `values.length` × 1 box, y growing down.
 */
export function scoreLineSegments(
  values: readonly (number | null)[],
  [min, max]: [number, number],
): { x: number; y: number }[][] {
  const segments: { x: number; y: number }[][] = [];
  let current: { x: number; y: number }[] = [];
  values.forEach((value, index) => {
    if (value === null) {
      if (current.length > 0) {
        segments.push(current);
      }
      current = [];
      return;
    }
    current.push({ x: index + 0.5, y: 1 - (value - min) / (max - min) });
  });
  if (current.length > 0) {
    segments.push(current);
  }
  return segments;
}

/** A score per hour as a line, with a dot on each hour that has a value. */
export function ScoreLine({
  values,
  boolean,
  label,
  className,
  axis = false,
  format = (value: number) => value.toFixed(2),
}: {
  values: readonly (number | null)[];
  boolean: boolean;
  label: string;
  className?: string;
  /** Shows the y range and the time span around the line. */
  axis?: boolean;
  format?: (value: number) => string;
}) {
  const domain = scoreLineDomain(values, boolean);
  const segments = scoreLineSegments(values, domain);
  const width = values.length;
  // The box is `width` × 1 stretched to the element, so the dots are drawn as zero-length lines
  // with round caps: they keep their size whatever the stretch.
  const line = (
    <svg
      role="img"
      aria-label={label}
      viewBox={`0 -0.08 ${width} 1.16`}
      preserveAspectRatio="none"
      className={cn("h-4 w-16 overflow-visible text-primary", className)}
    >
      {segments.map((points) => (
        <g key={points[0]?.x}>
          {points.length > 1 ? (
            <polyline
              points={points.map((point) => `${point.x},${point.y}`).join(" ")}
              fill="none"
              stroke="currentColor"
              strokeWidth={1.5}
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
          ) : null}
          {points.map((point) => (
            <line
              key={point.x}
              x1={point.x}
              x2={point.x}
              y1={point.y}
              y2={point.y}
              stroke="currentColor"
              strokeWidth={axis ? 4 : 2.5}
              strokeLinecap="round"
              vectorEffect="non-scaling-stroke"
            />
          ))}
        </g>
      ))}
    </svg>
  );
  if (!axis) {
    return line;
  }
  const [min, max] = domain;
  return (
    <div className="grid grid-cols-[auto_1fr] gap-x-2 text-caption text-muted-foreground tabular-nums">
      <div className="flex flex-col justify-between text-right">
        <span>{format(max)}</span>
        <span>{format(min)}</span>
      </div>
      <div className="border-b border-l">{line}</div>
      <span />
      <div className="flex justify-between">
        <span>24 h ago</span>
        <span>now</span>
      </div>
    </div>
  );
}
