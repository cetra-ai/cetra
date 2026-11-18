import { render, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ChartRow } from "@/lib/metrics";
import { WidgetPlot } from "./widget-plot";

const START = Date.UTC(2026, 0, 1, 0, 0);

/** A ResizeObserver that reports every observed element at `width` once it is observed. */
class FixedWidthObserver {
  constructor(
    private readonly callback: ResizeObserverCallback,
    private readonly width: number,
  ) {}
  observe(target: Element): void {
    this.callback(
      [
        {
          target,
          contentRect: { width: this.width, height: 160 },
        } as unknown as ResizeObserverEntry,
      ],
      this as unknown as ResizeObserver,
    );
  }
  unobserve(): void {}
  disconnect(): void {}
}

describe("WidgetPlot", () => {
  it("labels a 160 px mini chart with at most two X-axis ticks", async () => {
    vi.stubGlobal(
      "ResizeObserver",
      class extends FixedWidthObserver {
        constructor(callback: ResizeObserverCallback) {
          super(callback, 160);
        }
      },
    );
    try {
      // A day of ten-minute buckets: far more round times than a 160 px chart can label.
      const rows: ChartRow[] = Array.from({ length: 144 }, (_, index) => ({
        time: START + index * 600_000,
        score_0: index,
      }));
      const { container } = render(
        <WidgetPlot
          title="Scores"
          kind="line"
          stacked={false}
          columns={[
            { key: "score_0", label: "Score", color: "var(--chart-1)", kind: "count", group: null },
          ]}
          rows={rows}
          bucketNs={600_000_000_000}
        />,
      );
      const selector = ".recharts-xAxis .recharts-cartesian-axis-tick";
      await waitFor(() => expect(container.querySelectorAll(selector)).not.toHaveLength(0));
      // 160 px less the 84 px of the value axis and its margin: 76 px, so two labels at most.
      expect(container.querySelectorAll(selector).length).toBeLessThanOrEqual(2);
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
