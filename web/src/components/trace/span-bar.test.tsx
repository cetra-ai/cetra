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

function bar(leftPct: number, widthPct: number, timelineWidth = 400) {
  render(
    <TooltipProvider>
      <div style={{ position: "relative" }}>
        <SpanBar
          span={SPAN}
          bar={{ leftPct, widthPct, clipStart: false, clipEnd: false }}
          colorClass="bg-primary"
          timelineWidth={timelineWidth}
          traceStartNs={TRACE_START}
          onDoubleClick={() => {}}
        />
      </div>
    </TooltipProvider>,
  );
}

const placement = () => document.querySelector("[data-placement]")?.getAttribute("data-placement");

it("writes the duration to the right of the bar when there is room", () => {
  bar(10, 20);
  expect(placement()).toBe("right");
  expect(document.querySelector("[data-placement]")).toHaveTextContent("500ms");
});

it("writes it to the left when the bar ends near the right edge", () => {
  bar(60, 38);
  expect(placement()).toBe("left");
});

it("writes it inside the bar when the bar spans the whole width", () => {
  bar(0, 100);
  expect(placement()).toBe("inside");
});

it("shows the start, end and total time in a tooltip", async () => {
  bar(10, 20);
  fireEvent.focus(screen.getByRole("button", { name: "Timeline get_order" }));
  const tip = await screen.findByRole("tooltip");
  expect(tip).toHaveTextContent("get_order");
  expect(tip).toHaveTextContent(/Start \d\d:\d\d:\d\d\.500 \(\+1\.50s\)/);
  expect(tip).toHaveTextContent(/End \d\d:\d\d:\d\d\.000 \(\+2\.00s\)/);
  expect(tip).toHaveTextContent("Duration 500ms");
});
