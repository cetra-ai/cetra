import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { SpanBar } from "@/components/trace/span-bar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { instantNs } from "@/lib/format";

const SPAN = {
  name: "get_order",
  span_kind: "tool",
  status: "ok",
  start_time: "2026-01-01T11:00:01.500Z",
  end_time: "2026-01-01T11:00:02.000Z",
  duration_ns: 500_000_000,
} as Schema<"TreeSpan">["span"];

const TRACE_START = instantNs("2026-01-01T11:00:00.000Z");

function bar(
  leftPct: number,
  widthPct: number,
  timelineWidth = 400,
  extra: Partial<Schema<"TreeSpan">["span"]> = {},
) {
  render(
    <TooltipProvider>
      <div style={{ position: "relative" }}>
        <SpanBar
          span={{ ...SPAN, ...extra }}
          bar={{ leftPct, widthPct, clipStart: false, clipEnd: false }}
          colorClass="bg-primary"
          timelineWidth={timelineWidth}
          traceStartNs={TRACE_START}
          selfNs={200_000_000}
          onDoubleClick={() => {}}
        />
      </div>
    </TooltipProvider>,
  );
}

const placement = () => document.querySelector("[data-placement]")?.getAttribute("data-placement");

const label = () => document.querySelector("[data-placement]");

it("writes the duration inside the bar when it fits", () => {
  bar(0, 100);
  expect(placement()).toBe("inside");
  expect(label()).toHaveTextContent("500ms");
});

it("adds the cost after the duration when the span has one", () => {
  bar(0, 100, 400, { cost_total: 0.0079 });
  expect(label()?.textContent).toBe("500ms  $0.0079");
});

it("writes it to the right of a short bar", () => {
  bar(10, 2);
  expect(placement()).toBe("right");
});

it("writes it to the left of a short bar near the end of the trace", () => {
  bar(95, 4);
  expect(placement()).toBe("left");
});

it("shows the start, end and total time in a tooltip", async () => {
  bar(10, 20);
  fireEvent.focus(screen.getByRole("button", { name: "Timeline get_order" }));
  const tip = await screen.findByRole("tooltip");
  expect(tip).toHaveTextContent("get_order");
  expect(tip).toHaveTextContent(/Start \d\d:\d\d:\d\d\.500 \(\+1\.50s\)/);
  expect(tip).toHaveTextContent(/End \d\d:\d\d:\d\d\.000 \(\+2\.00s\)/);
  expect(tip).toHaveTextContent("Duration 500ms");
  expect(tip).toHaveTextContent("Self time 200ms");
});
