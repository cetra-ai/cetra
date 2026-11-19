import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";
import { server } from "@/mocks/node";
import { Widget, type WidgetContext } from "./widget";

/** How many times the chart was drawn; the chart itself is the real one. */
const plot = vi.hoisted(() => ({ renders: 0 }));

vi.mock("./widget-plot", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./widget-plot")>();
  function CountedPlot(props: Parameters<typeof actual.WidgetPlot>[0]) {
    plot.renders += 1;
    return <actual.WidgetPlot {...props} />;
  }
  return { ...actual, WidgetPlot: CountedPlot };
});

const CONTEXT: WidgetContext = {
  org: "acme",
  project: "support-bot",
  from: "2026-01-01T10:00:00.000Z",
  to: "2026-01-01T11:00:00.000Z",
  includeEval: false,
  includeExperiments: false,
};

const WIDGET: WidgetModel = {
  id: "w1",
  title: "Traces",
  kind: "chart",
  chart: { type: "bar" },
  query: { source: "traces", measures: [{ agg: "count" }], breakdown: { field: "name" } },
  layout: { x: 0, y: 0, w: 6, h: 7 },
};

function answerOneBucket() {
  server.use(
    http.post("/api/v1/projects/:project/dashboards/query", () =>
      HttpResponse.json({
        shape: "series",
        bucket_ns: 600_000_000_000,
        series: [
          {
            measure: 0,
            group: "alpha",
            other: false,
            points: [{ time: "2026-01-01T10:00:00.000000000Z", value: 2, rows: 2 }],
          },
        ],
      }),
    ),
  );
}

function setup() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrap = (node: ReactNode) => (
    <QueryClientProvider client={client}>
      <TooltipProvider>{node}</TooltipProvider>
    </QueryClientProvider>
  );
  return wrap;
}

async function bucket(): Promise<HTMLElement> {
  return waitFor(() => {
    const found = document.querySelector('[aria-label="Open bucket 2026-01-01T10:00:00.000Z"]');
    expect(found).toBeInstanceOf(Element);
    return found as HTMLElement;
  });
}

describe("a widget's redraws", () => {
  it("does not redraw its chart when only its place or the callbacks change", async () => {
    answerOneBucket();
    const wrap = setup();
    const { rerender } = render(
      wrap(<Widget widget={WIDGET} context={CONTEXT} onBucket={vi.fn()} />),
    );
    await bucket();
    const drawn = plot.renders;

    rerender(
      wrap(
        <Widget
          widget={{ ...WIDGET, layout: { x: 6, y: 3, w: 4, h: 5 } }}
          context={{ ...CONTEXT }}
          onBucket={vi.fn()}
        />,
      ),
    );
    expect(plot.renders).toBe(drawn);

    // A change to what it shows does redraw it.
    rerender(
      wrap(
        <Widget widget={{ ...WIDGET, title: "Renamed" }} context={CONTEXT} onBucket={vi.fn()} />,
      ),
    );
    expect(plot.renders).toBeGreaterThan(drawn);
  });

  it("calls the newest callback after a re-render that skipped the chart", async () => {
    answerOneBucket();
    const wrap = setup();
    const first = vi.fn();
    const second = vi.fn();
    const { rerender } = render(
      wrap(<Widget widget={WIDGET} context={CONTEXT} onBucket={first} />),
    );
    await bucket();

    rerender(wrap(<Widget widget={WIDGET} context={CONTEXT} onBucket={second} />));
    fireEvent.click(await bucket());

    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledTimes(1);
  });

  it("drops a callback that is no longer given", async () => {
    answerOneBucket();
    const wrap = setup();
    const { rerender } = render(
      wrap(<Widget widget={WIDGET} context={CONTEXT} onBucket={vi.fn()} />),
    );
    await bucket();

    rerender(wrap(<Widget widget={WIDGET} context={CONTEXT} />));

    await waitFor(() => {
      expect(
        document.querySelector('[aria-label="Open bucket 2026-01-01T10:00:00.000Z"]'),
      ).toBeNull();
    });
  });
});
