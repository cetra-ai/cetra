import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { LiveToggle } from "@/components/live-toggle";
import { startAnalytics, type WireEvent } from "@/lib/analytics";

it("flipping the switch is tracked", async () => {
  const sent: WireEvent[][] = [];
  const stop = startAnalytics({
    send: async (events) => {
      sent.push(events);
    },
    flushMs: 60_000,
  });
  const onPressedChange = vi.fn();
  try {
    const user = userEvent.setup();
    render(
      <LiveToggle
        pressed={false}
        disabled={false}
        status="closed"
        onPressedChange={onPressedChange}
      />,
    );
    await user.click(screen.getByRole("switch", { name: "Live" }));
  } finally {
    stop();
  }
  expect(onPressedChange).toHaveBeenCalledWith(true);
  expect(sent.flat()).toEqual([{ name: "live_tail.toggled", properties: { on: true } }]);
});
