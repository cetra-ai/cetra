import { describe, expect, it } from "vitest";
import {
  barGeometry,
  clampView,
  formatTick,
  labelPlacement,
  panBy,
  SLOW_MAX_SPANS,
  SLOW_SELF_SHARE,
  selfTimes,
  slowSpans,
  ticks,
  traceWindow,
  zoomAt,
  zoomToSpan,
} from "@/lib/timeline";

const START = "2026-01-01T00:00:00.000000000Z";
const ONE_SECOND = "2026-01-01T00:00:01.000000000Z";
const HALF = "2026-01-01T00:00:00.500000000Z";
const BEFORE = "2025-12-31T23:59:59.000000000Z";
const AFTER = "2026-01-01T00:00:02.000000000Z";

it("widens a zero-width trace to one microsecond and includes span extremes", () => {
  const point = traceWindow({ start_time: START, end_time: START }, []);
  // instantNs is a JS number. Around 2026 its step is 256 ns, so a 1 µs pad
  // lands on the next representable instant rather than exactly 1 000 ns.
  expect(point.endNs - point.startNs).toBeGreaterThanOrEqual(1_000);
  expect(point.endNs - point.startNs).toBeLessThan(2_000);

  const wider = traceWindow({ start_time: START, end_time: ONE_SECOND }, [
    { span: { start_time: BEFORE, end_time: AFTER } },
  ]);
  expect(wider.startNs).toBeLessThan(
    traceWindow({ start_time: START, end_time: ONE_SECOND }, []).startNs,
  );
  expect(wider.endNs).toBeGreaterThan(
    traceWindow({ start_time: START, end_time: ONE_SECOND }, []).endNs,
  );
});

it("clamps bars to the window and keeps a sliver for a zero-duration span", () => {
  const window = traceWindow({ start_time: START, end_time: ONE_SECOND }, []);
  expect(barGeometry({ start_time: START, end_time: ONE_SECOND }, window)).toEqual({
    leftPct: 0,
    widthPct: 100,
    clipStart: false,
    clipEnd: false,
  });

  const zero = barGeometry({ start_time: HALF, end_time: HALF }, window);
  if (zero === null) {
    throw new Error("expected a bar for a span inside the window");
  }
  expect(zero.widthPct).toBeCloseTo(0.15);
  expect(zero.leftPct).toBeGreaterThan(40);
  expect(zero.leftPct).toBeLessThan(60);
  expect(zero.leftPct + zero.widthPct).toBeLessThanOrEqual(100);

  const early = barGeometry({ start_time: BEFORE, end_time: BEFORE }, window);
  expect(early).toBeNull();

  const late = barGeometry({ start_time: AFTER, end_time: AFTER }, window);
  expect(late).toBeNull();
});

const SECOND = 1_000_000_000;
const MS = 1_000_000;

describe("ticks", () => {
  it.each([
    [6.2 * SECOND, ["0s", "1s", "2s", "3s", "4s", "5s", "6s"]],
    [1 * SECOND, ["0ms", "200ms", "400ms", "600ms", "800ms", "1s"]],
    [350 * MS, ["0ms", "50ms", "100ms", "150ms", "200ms", "250ms", "300ms", "350ms"]],
    [12 * SECOND, ["0s", "2s", "4s", "6s", "8s", "10s", "12s"]],
    [120 * SECOND, ["0s", "20s", "40s", "1m", "1m 20s", "1m 40s", "2m"]],
    [17 * SECOND, ["0s", "2.5s", "5s", "7.5s", "10s", "12.5s", "15s"]],
  ])("puts 6 to 8 round ticks on a %d ns trace", (span, labels) => {
    const marks = ticks({ startNs: 0, endNs: span }, 0, 900);
    expect(marks.map((mark) => mark.label)).toEqual(labels);
  });

  it("keeps every trace length between 5 and 9 round ticks", () => {
    for (let span = 1_000; span < 3_600 * SECOND; span *= 1.37) {
      const marks = ticks({ startNs: 0, endNs: span }, 0, 900);
      expect(marks.length).toBeGreaterThanOrEqual(5);
      expect(marks.length).toBeLessThanOrEqual(9);
      const [first, second] = marks;
      expect(first?.ns).toBe(0);
      expect(isNice((second?.ns ?? 0) - (first?.ns ?? 0))).toBe(true);
    }
  });

  it("counts from the trace start when zoomed, and only inside the view", () => {
    const origin = 1_700_000_000 * SECOND;
    const marks = ticks(
      { startNs: origin + 1.05 * SECOND, endNs: origin + 2.05 * SECOND },
      origin,
      900,
    );
    expect(marks.map((mark) => mark.label)).toEqual(["1.2s", "1.4s", "1.6s", "1.8s", "2s"]);
  });

  it("thins the ticks on a narrow axis", () => {
    const marks = ticks({ startNs: 0, endNs: 6.2 * SECOND }, 0, 200);
    expect(marks.map((mark) => mark.label)).toEqual(["0s", "2s", "4s", "6s"]);
  });

  it("formats an offset in the step's unit", () => {
    expect(formatTick(0, 500 * MS)).toBe("0ms");
    expect(formatTick(1_500 * MS, 500 * MS)).toBe("1.5s");
    expect(formatTick(1_050 * MS, 50 * MS)).toBe("1.05s");
    expect(formatTick(2 * SECOND, 500 * MS)).toBe("2s");
    expect(formatTick(90 * SECOND, 10 * SECOND)).toBe("1m 30s");
    expect(formatTick(250_000, 50_000)).toBe("250µs");
    expect(formatTick(400, 100)).toBe("400ns");
  });
});

function row(id: string, depth: number, startMs: number, endMs: number) {
  const at = (ms: number) => new Date(Date.UTC(2026, 0, 1) + ms).toISOString();
  return { depth, span: { span_id: id, start_time: at(startMs), end_time: at(endMs) } };
}

describe("selfTimes", () => {
  it("subtracts the direct children, counting overlapping children once", () => {
    const spans = [
      row("root", 0, 0, 1_000),
      row("a", 1, 0, 400),
      row("a1", 2, 100, 300),
      row("b", 1, 300, 600),
      row("c", 1, 900, 1_200),
    ];
    // root: a, b and c cover 0–600 and 900–1000 → 300ms; a: 200ms; c runs past root's end.
    // Epoch nanoseconds lose a few hundred ns of precision as numbers, so compare in ms.
    expect([...selfTimes(spans)].map((ns) => Math.round(ns / MS))).toEqual([
      300, 200, 200, 300, 300,
    ]);
  });

  it("gives a leaf its whole duration and never goes below zero", () => {
    expect([...selfTimes([row("leaf", 0, 0, 50)])].map((ns) => Math.round(ns / MS))).toEqual([50]);
    expect([...selfTimes([row("back", 0, 50, 0)])]).toEqual([0]);
  });
});

describe("slowSpans", () => {
  const trace = 1_000 * MS;
  it("flags self time above 20 % of the trace, never the root, boundary excluded", () => {
    expect(SLOW_SELF_SHARE).toBe(0.2);
    const spans = [
      row("root", 0, 0, 1_000),
      row("parent", 1, 0, 500),
      row("child", 2, 0, 400),
      row("edge", 1, 500, 700),
      row("over", 1, 700, 901),
    ];
    // parent: 100ms self, child 400ms, edge exactly 200ms, over 201ms.
    expect(slowSpans(spans, selfTimes(spans), trace)).toEqual(new Set(["child", "over"]));
  });

  it("keeps only the three largest, so parallel work isn't all flagged", () => {
    expect(SLOW_MAX_SPANS).toBe(3);
    const spans = [
      row("root", 0, 0, 1_000),
      row("p1", 1, 0, 300),
      row("p2", 1, 0, 500),
      row("p3", 1, 0, 400),
      row("p4", 1, 0, 600),
      row("p5", 1, 0, 250),
    ];
    expect(slowSpans(spans, selfTimes(spans), trace)).toEqual(new Set(["p4", "p2", "p3"]));
  });

  it("flags nothing on an empty trace", () => {
    const spans = [row("root", 0, 0, 0), row("a", 1, 0, 0)];
    expect(slowSpans(spans, selfTimes(spans), 0).size).toBe(0);
  });
});

describe("labelPlacement", () => {
  it("goes inside a bar it fits in", () => {
    expect(labelPlacement({ leftPct: 0, widthPct: 100 }, 80, 600)).toBe("inside");
    expect(labelPlacement({ leftPct: 40, widthPct: 20 }, 120, 600)).toBe("inside");
  });
  it("goes right of a short bar with room after it", () => {
    expect(labelPlacement({ leftPct: 10, widthPct: 2 }, 80, 600)).toBe("right");
  });
  it("goes left of a short bar near the end of the trace", () => {
    expect(labelPlacement({ leftPct: 92, widthPct: 7 }, 110, 600)).toBe("left");
    expect(labelPlacement({ leftPct: 98, widthPct: 1 }, 50, 600)).toBe("left");
  });
  it("stays inside when no side has room", () => {
    expect(labelPlacement({ leftPct: 5, widthPct: 90 }, 300, 200)).toBe("inside");
  });
});

const TRACE = { startNs: 0, endNs: 1_000_000_000 };

it("zooms about an anchor, pans, and stays inside the trace", () => {
  const zoomed = zoomAt(TRACE, TRACE, 50, 2);
  expect(zoomed).toEqual({ startNs: 250_000_000, endNs: 750_000_000 });
  expect(zoomAt(TRACE, zoomed, 0, 0.5)).toEqual(TRACE);
  expect(zoomAt(TRACE, TRACE, 50, 0)).toEqual(TRACE);

  const panned = panBy(TRACE, { startNs: 0, endNs: 400_000_000 }, 50);
  expect(panned).toEqual({ startNs: 200_000_000, endNs: 600_000_000 });
  expect(panBy(TRACE, { startNs: 800_000_000, endNs: 1_000_000_000 }, 100)).toEqual({
    startNs: 800_000_000,
    endNs: 1_000_000_000,
  });

  expect(clampView(TRACE, { startNs: -50, endNs: 100 })).toEqual({
    startNs: 0,
    endNs: 1_000,
  });
  expect(clampView(TRACE, { startNs: 900_000_000, endNs: 1_200_000_000 }).endNs).toBe(TRACE.endNs);
});

it("zooms to a span with five percent padding on each side", () => {
  const span = {
    start_time: "1970-01-01T00:00:00.100000000Z",
    end_time: "1970-01-01T00:00:00.200000000Z",
  };
  expect(zoomToSpan(TRACE, span)).toEqual({
    startNs: 95_000_000,
    endNs: 205_000_000,
  });
});

it("clips a bar that crosses the view and hides one that misses it", () => {
  const view = { startNs: 1_000_000_000, endNs: 2_000_000_000 };
  const crossing = barGeometry(
    {
      start_time: "1970-01-01T00:00:00.500000000Z",
      end_time: "1970-01-01T00:00:01.500000000Z",
    },
    view,
  );
  expect(crossing).toEqual({ leftPct: 0, widthPct: 50, clipStart: true, clipEnd: false });
  const leaving = barGeometry(
    {
      start_time: "1970-01-01T00:00:01.500000000Z",
      end_time: "1970-01-01T00:00:02.500000000Z",
    },
    view,
  );
  expect(leaving).toEqual({ leftPct: 50, widthPct: 50, clipStart: false, clipEnd: true });
});

function isNice(step: number): boolean {
  const exponent = Math.floor(Math.log10(step));
  const base = step / 10 ** exponent;
  return [1, 2, 2.5, 5].some((nice) => Math.abs(base - nice) < 1e-6);
}
