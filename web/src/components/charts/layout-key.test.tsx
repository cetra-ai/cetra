import { render } from "@testing-library/react";
import { Bar, BarChart } from "recharts";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ChartContainer } from "@/components/ui/chart";
import { LayoutKey } from "./layout-key";

/** jsdom lays nothing out: every element measures `size`. */
function measureAs(size: { width: number; height: number }) {
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(() => size.width);
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(() => size.height);
}

afterEach(() => {
  vi.restoreAllMocks();
});

function chart(layoutKey?: string) {
  const body = (
    <ChartContainer config={{ value: { label: "Value" } }}>
      <BarChart data={[{ value: 1 }]}>
        <Bar dataKey="value" isAnimationActive={false} />
      </BarChart>
    </ChartContainer>
  );
  return layoutKey === undefined ? (
    body
  ) : (
    <LayoutKey.Provider value={layoutKey}>{body}</LayoutKey.Provider>
  );
}

function responsiveBox(container: HTMLElement): HTMLElement | null {
  return container.querySelector<HTMLElement>(".recharts-responsive-container");
}

/** The width the chart is drawn at. */
function drawnWidth(container: HTMLElement): string | null | undefined {
  return container.querySelector("svg.recharts-surface")?.getAttribute("width");
}

describe("a chart under a layout key", () => {
  it("draws at the size its box measures as it mounts", () => {
    const size = { width: 500, height: 200 };
    measureAs(size);
    const { container } = render(chart("a"));
    // Recharts draws at the given size, with no box of its own to measure.
    expect(responsiveBox(container)).toBeNull();
    expect(drawnWidth(container)).toBe("500");
    expect(container.querySelector("svg.recharts-surface")?.getAttribute("height")).toBe("200");
  });

  it("measures again in the very render that changes the key", () => {
    const size = { width: 500, height: 200 };
    measureAs(size);
    const { container, rerender } = render(chart("a"));
    size.width = 400;
    // The same key: nothing asks for a measure, so the size stays until an observer says so.
    rerender(chart("a"));
    expect(drawnWidth(container)).toBe("500");
    // A new key: measured before the render returns, with no frame at the old size.
    rerender(chart("b"));
    expect(drawnWidth(container)).toBe("400");
  });

  it("leaves the size to Recharts while the box measures empty", () => {
    measureAs({ width: 0, height: 0 });
    const { container } = render(chart("a"));
    expect(responsiveBox(container)).not.toBeNull();
  });
});

describe("a chart outside a layout key", () => {
  it("lets Recharts measure its box, as before", () => {
    measureAs({ width: 500, height: 200 });
    const { container } = render(chart());
    expect(responsiveBox(container)).not.toBeNull();
  });
});
