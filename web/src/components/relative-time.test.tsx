import { render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { RelativeTime } from "@/components/relative-time";

afterEach(() => {
  vi.useRealTimers();
});

it("shows the time as relative to now, with the exact time on the element", () => {
  vi.useFakeTimers({ now: Date.parse("2026-11-17T12:00:00.000Z") });
  const value = "2026-11-17T10:00:00.000Z";
  render(<RelativeTime value={value} />);
  const time = screen.getByText("2h ago");
  expect(time.tagName).toBe("TIME");
  expect(time.getAttribute("datetime")).toBe(value);
});
