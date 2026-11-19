import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useDelayedFlag } from "@/lib/use-delayed-flag";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

it("stays false until the flag has been active for the delay", () => {
  const { result } = renderHook(({ active }) => useDelayedFlag(active, 200), {
    initialProps: { active: true },
  });
  expect(result.current).toBe(false);
  act(() => {
    vi.advanceTimersByTime(199);
  });
  expect(result.current).toBe(false);
  act(() => {
    vi.advanceTimersByTime(1);
  });
  expect(result.current).toBe(true);
});

it("turns false at once when the flag goes inactive", () => {
  const { result, rerender } = renderHook(({ active }) => useDelayedFlag(active, 200), {
    initialProps: { active: true },
  });
  act(() => {
    vi.advanceTimersByTime(200);
  });
  expect(result.current).toBe(true);
  rerender({ active: false });
  expect(result.current).toBe(false);
});

it("restarts the wait when the flag goes inactive and active again", () => {
  const { result, rerender } = renderHook(({ active }) => useDelayedFlag(active, 200), {
    initialProps: { active: true },
  });
  act(() => {
    vi.advanceTimersByTime(150);
  });
  rerender({ active: false });
  rerender({ active: true });
  act(() => {
    vi.advanceTimersByTime(150);
  });
  expect(result.current).toBe(false);
  act(() => {
    vi.advanceTimersByTime(50);
  });
  expect(result.current).toBe(true);
});

it("never turns true when the flag is inactive", () => {
  const { result } = renderHook(() => useDelayedFlag(false));
  act(() => {
    vi.advanceTimersByTime(1_000);
  });
  expect(result.current).toBe(false);
});

it("uses 200ms when no delay is given", () => {
  const { result } = renderHook(() => useDelayedFlag(true));
  act(() => {
    vi.advanceTimersByTime(200);
  });
  expect(result.current).toBe(true);
});
