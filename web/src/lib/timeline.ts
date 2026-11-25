import { instantNs } from "@/lib/format";
import { subtreeEnds } from "@/lib/tree";

/** One microsecond. A window narrower than this is widened so bars have a scale. */
const MIN_WINDOW_NS = 1_000;
const MIN_BAR_PCT = 0.15;

export type TimeWindow = {
  startNs: number;
  endNs: number;
};

export function traceWindow(
  trace: { start_time: string; end_time: string },
  spans: readonly { span: { start_time: string; end_time: string } }[],
): TimeWindow {
  let startNs = instantNs(trace.start_time);
  let endNs = instantNs(trace.end_time);
  for (const row of spans) {
    startNs = Math.min(startNs, instantNs(row.span.start_time));
    endNs = Math.max(endNs, instantNs(row.span.end_time));
  }
  if (!(endNs - startNs >= MIN_WINDOW_NS)) {
    endNs = startNs + MIN_WINDOW_NS;
  }
  return { startNs, endNs };
}

export type BarGeometry = {
  leftPct: number;
  widthPct: number;
  /** The span starts before the view. */
  clipStart: boolean;
  /** The span ends after the view. */
  clipEnd: boolean;
};

/**
 * Bar position inside the view window.
 * A span that misses the view is hidden. A span that crosses an edge is clipped.
 * A zero-duration span inside the view keeps a sliver of width.
 */
export function barGeometry(
  span: { start_time: string; end_time: string },
  view: TimeWindow,
): BarGeometry | null {
  const spanNs = view.endNs - view.startNs;
  if (!(spanNs > 0)) {
    return null;
  }
  const start = instantNs(span.start_time);
  const end = instantNs(span.end_time);
  if (end <= view.startNs || start >= view.endNs) {
    return null;
  }
  const clipStart = start < view.startNs;
  const clipEnd = end > view.endNs;
  const rawLeft = ((start - view.startNs) / spanNs) * 100;
  const rawRight = ((end - view.startNs) / spanNs) * 100;
  let left = clamp(rawLeft, 0, 100);
  let right = clamp(rawRight, 0, 100);
  if (right < left) {
    right = left;
  }
  let width = right - left;
  if (width < MIN_BAR_PCT) {
    width = MIN_BAR_PCT;
    if (left + width > 100) {
      left = 100 - MIN_BAR_PCT;
    }
  }
  return { leftPct: left, widthPct: width, clipStart, clipEnd };
}

/** Keep `view` inside the trace window. The narrowest view is one microsecond. */
export function clampView(trace: TimeWindow, view: TimeWindow): TimeWindow {
  const span = trace.endNs - trace.startNs;
  const minWidth = Math.min(span, MIN_WINDOW_NS);
  let width = view.endNs - view.startNs;
  if (!(width >= minWidth)) {
    width = minWidth;
  }
  if (width > span) {
    width = span;
  }
  let start = Number.isFinite(view.startNs) ? view.startNs : trace.startNs;
  if (start < trace.startNs) {
    start = trace.startNs;
  }
  if (start + width > trace.endNs) {
    start = trace.endNs - width;
  }
  return { startNs: start, endNs: start + width };
}

/**
 * Zoom about a point. `factor` above 1 narrows the view; below 1 widens it.
 * `anchorPct` is the pointer's place in the current view, 0–100, and it stays put.
 */
export function zoomAt(
  trace: TimeWindow,
  view: TimeWindow,
  anchorPct: number,
  factor: number,
): TimeWindow {
  const current = clampView(trace, view);
  if (!(factor > 0) || !Number.isFinite(factor)) {
    return current;
  }
  const width = current.endNs - current.startNs;
  const pct = clamp(anchorPct, 0, 100) / 100;
  const anchor = current.startNs + pct * width;
  const nextWidth = width / factor;
  return clampView(trace, {
    startNs: anchor - pct * nextWidth,
    endNs: anchor - pct * nextWidth + nextWidth,
  });
}

/** Move the view by a percentage of its width. Positive goes later. */
export function panBy(trace: TimeWindow, view: TimeWindow, deltaPct: number): TimeWindow {
  const current = clampView(trace, view);
  const width = current.endNs - current.startNs;
  const delta = (deltaPct / 100) * width;
  return clampView(trace, {
    startNs: current.startNs + delta,
    endNs: current.endNs + delta,
  });
}

/** The span, plus 5 % of its duration on each side, clamped to the trace. */
export function zoomToSpan(
  trace: TimeWindow,
  span: { start_time: string; end_time: string },
): TimeWindow {
  const start = instantNs(span.start_time);
  let end = instantNs(span.end_time);
  if (end < start) {
    end = start;
  }
  const width = Math.max(end - start, MIN_WINDOW_NS);
  const pad = width * 0.05;
  return clampView(trace, { startNs: start - pad, endNs: start + width + pad });
}

/** The fewest and most ticks the axis aims for, and the room each one needs. */
const TARGET_TICKS = 7;
const MIN_TICK_SPACING_PX = 64;
// Round steps: 1, 2, 2.5 and 5 times a power of ten.
const TICK_BASES = [1, 2, 2.5, 5];

/**
 * Round ticks (1s, 500ms, 200ms, …) inside `view`, at multiples of the step counted from
 * `originNs` (the trace start), each labelled with its offset from the origin. The step aims
 * for about seven ticks across the view, fewer when `widthPx` leaves under 64px between them.
 */
export function ticks(
  view: TimeWindow,
  originNs: number,
  widthPx: number,
): { ns: number; label: string }[] {
  const span = view.endNs - view.startNs;
  if (!(span > 0)) {
    return [];
  }
  const room = Math.max(2, Math.floor(widthPx / MIN_TICK_SPACING_PX) + 1);
  const step = tickStep(span, Math.min(TARGET_TICKS, room));
  // Offsets from the origin stay small, so they keep the precision epoch nanoseconds lack.
  const from = view.startNs - originNs;
  const to = view.endNs - originNs;
  const marks: { ns: number; label: string }[] = [];
  for (let index = Math.ceil(from / step - 1e-6); index * step <= to + step * 1e-6; index++) {
    const offset = index * step;
    marks.push({ ns: originNs + offset, label: formatTick(offset, step) });
  }
  return marks;
}

/** The round step whose tick count over `span` is closest to `target`. */
export function tickStep(span: number, target: number): number {
  const exponent = Math.floor(Math.log10(span));
  let best = 10 ** exponent;
  let bestScore = Number.POSITIVE_INFINITY;
  for (let power = exponent - 2; power <= exponent; power++) {
    for (const base of TICK_BASES) {
      const step = base * 10 ** power;
      const count = Math.floor(span / step + 1e-9) + 1;
      const score = Math.abs(count - target);
      if (score < bestScore) {
        bestScore = score;
        best = step;
      }
    }
  }
  return best;
}

/** A tick label: the offset in the step's unit, without trailing zeros ("2.5s", "500ms", "1m 30s"). */
export function formatTick(offsetNs: number, stepNs: number): string {
  const trim = (value: number) => String(Number(value.toFixed(3)));
  if (stepNs >= 1_000_000_000) {
    const seconds = offsetNs / 1_000_000_000;
    if (seconds >= 60) {
      const minutes = Math.floor(seconds / 60);
      const rest = seconds - minutes * 60;
      return rest > 0 ? `${minutes}m ${trim(rest)}s` : `${minutes}m`;
    }
    return `${trim(seconds)}s`;
  }
  if (stepNs >= 1_000_000) {
    return offsetNs >= 1_000_000_000
      ? `${trim(offsetNs / 1_000_000_000)}s`
      : `${trim(offsetNs / 1_000_000)}ms`;
  }
  if (stepNs >= 1_000) {
    return `${trim(offsetNs / 1_000)}µs`;
  }
  return `${trim(offsetNs)}ns`;
}

// Placeholders to tune: a span is slow when its self time is above this share of the trace,
// and only the slowest few are flagged, so a trace with parallel work isn't covered in them.
export const SLOW_SELF_SHARE = 0.2;
export const SLOW_MAX_SPANS = 3;

type TimedRow = { depth: number; span: { start_time: string; end_time: string } };

/**
 * Each span's self time: its duration minus the time its direct children cover, with
 * overlapping (parallel) children counted once and clipped to the span.
 */
export function selfTimes(spans: readonly TimedRow[]): Float64Array {
  const self = new Float64Array(spans.length);
  const ends = subtreeEnds(spans);
  for (let index = 0; index < spans.length; index++) {
    const row = spans[index];
    if (!row) {
      continue;
    }
    const start = instantNs(row.span.start_time);
    const end = Math.max(start, instantNs(row.span.end_time));
    const children: [number, number][] = [];
    const last = ends[index] ?? index + 1;
    for (let child = index + 1; child < last; child++) {
      const node = spans[child];
      if (node?.depth !== row.depth + 1) {
        continue;
      }
      const from = Math.max(start, instantNs(node.span.start_time));
      const to = Math.min(end, instantNs(node.span.end_time));
      if (to > from) {
        children.push([from, to]);
      }
    }
    children.sort((left, right) => left[0] - right[0]);
    // Merge the sorted intervals into runs; each run is counted once.
    let covered = 0;
    let runStart = start;
    let runEnd = start;
    for (const [from, to] of children) {
      if (from > runEnd) {
        covered += runEnd - runStart;
        runStart = from;
        runEnd = to;
      } else {
        runEnd = Math.max(runEnd, to);
      }
    }
    covered += runEnd - runStart;
    self[index] = Math.max(0, end - start - covered);
  }
  return self;
}

/**
 * The spans to flag as slow: at most three, the largest self times above 20 % of the trace.
 * Roots are never flagged.
 */
export function slowSpans(
  spans: readonly (TimedRow & { span: { span_id: string } })[],
  self: Float64Array,
  traceNs: number,
): Set<string> {
  if (!(traceNs > 0)) {
    return new Set();
  }
  const candidates: number[] = [];
  for (let index = 0; index < spans.length; index++) {
    const row = spans[index];
    if (row && row.depth > 0 && (self[index] ?? 0) / traceNs > SLOW_SELF_SHARE) {
      candidates.push(index);
    }
  }
  candidates.sort((left, right) => (self[right] ?? 0) - (self[left] ?? 0) || left - right);
  return new Set(
    candidates.slice(0, SLOW_MAX_SPANS).map((index) => spans[index]?.span.span_id ?? ""),
  );
}

export type LabelPlacement = "inside" | "right" | "left";

/**
 * Where a bar's label goes: inside when it fits, else right of the bar when there is room,
 * else left of it (a span late in the trace), else inside anyway.
 */
export function labelPlacement(
  bar: { leftPct: number; widthPct: number },
  labelPx: number,
  timelineWidth: number,
): LabelPlacement {
  const startPx = (bar.leftPct / 100) * timelineWidth;
  const widthPx = (bar.widthPct / 100) * timelineWidth;
  const endPx = startPx + widthPx;
  if (labelPx <= widthPx) {
    return "inside";
  }
  if (endPx + labelPx <= timelineWidth) {
    return "right";
  }
  if (labelPx <= startPx) {
    return "left";
  }
  return "inside";
}

function clamp(value: number, min: number, max: number): number {
  if (Number.isNaN(value)) {
    return min;
  }
  return Math.min(max, Math.max(min, value));
}

/** The arrowed ends of a bar that continues past the view. */
export function barClipPath(bar: BarGeometry): string | undefined {
  if (bar.clipStart && bar.clipEnd) {
    return "polygon(6px 0, calc(100% - 6px) 0, 100% 50%, calc(100% - 6px) 100%, 6px 100%, 0 50%)";
  }
  if (bar.clipStart) {
    return "polygon(6px 0, 100% 0, 100% 100%, 6px 100%, 0 50%)";
  }
  if (bar.clipEnd) {
    return "polygon(0 0, calc(100% - 6px) 0, 100% 50%, calc(100% - 6px) 100%, 0 100%)";
  }
  return undefined;
}
