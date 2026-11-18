import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { WidgetNumber } from "./widget-number";

describe("WidgetNumber", () => {
  it("wraps the change badge under the value when they do not fit side by side", () => {
    render(
      <TooltipProvider>
        <WidgetNumber
          label="Tokens: 1,644,679"
          kind="count"
          value={1_644_679}
          previous={1_200_000}
          tone="neutral"
        />
      </TooltipProvider>,
    );
    const badge = screen.getByText(/%$/);
    expect(badge).toBeInTheDocument();
    expect(badge.className).not.toMatch(/truncate|overflow-hidden|whitespace-nowrap/);
    const trigger = screen.getByRole("button");
    expect(trigger.className).toContain("flex-wrap");
    expect(trigger).toContainElement(badge);
  });
});
