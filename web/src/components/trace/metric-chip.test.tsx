import { render, screen } from "@testing-library/react";
import { Clock } from "lucide-react";
import { expect, it, vi } from "vitest";
import { MetricChip } from "@/components/trace/metric-chip";

it("shows the value with its icon hidden from assistive tech", () => {
  const { container } = render(
    <MetricChip icon={Clock} label="Start time">
      12:00:01
    </MetricChip>,
  );
  expect(screen.getByText("Start time")).toBeInTheDocument();
  expect(screen.getByText("12:00:01")).toBeInTheDocument();
  expect(container.querySelector("svg")).toHaveAttribute("aria-hidden", "true");
  expect(container.querySelector("svg")).toHaveClass("size-3.5", "text-muted-foreground");
});

it("forwards props and a ref to the chip element", () => {
  const onClick = vi.fn();
  const ref = { current: null as HTMLSpanElement | null };
  render(
    <MetricChip icon={Clock} label="Duration" ref={ref} onClick={onClick} data-testid="chip">
      1.2s
    </MetricChip>,
  );
  const chip = screen.getByTestId("chip");
  expect(chip).toHaveTextContent("Duration");
  expect(ref.current).toBe(chip);
  chip.click();
  expect(onClick).toHaveBeenCalledTimes(1);
});
