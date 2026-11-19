import { act, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useGridWidth } from "./grid-width";

/** A ResizeObserver whose callback the test calls itself. */
const observers = vi.hoisted(() => ({ callbacks: [] as ResizeObserverCallback[] }));

class ManualObserver {
  constructor(callback: ResizeObserverCallback) {
    observers.callbacks.push(callback);
  }
  observe() {}
  unobserve() {}
  disconnect() {}
}

/** Every width a render saw, in order. */
let seen: (number | null)[] = [];

function Probe() {
  const { width, containerRef } = useGridWidth();
  seen.push(width);
  return <div ref={containerRef}>{width === null ? "unmeasured" : width}</div>;
}

function resizeTo(width: number) {
  const entry = { contentRect: { width } } as ResizeObserverEntry;
  for (const callback of observers.callbacks) {
    callback([entry], {} as ResizeObserver);
  }
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  observers.callbacks = [];
  seen = [];
});

describe("the grid's width", () => {
  it("is measured before the first paint, with no render at a guessed width", () => {
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(900);
    const { container } = render(<Probe />);
    // The first render has no width; the next one, still before paint, has the measured one.
    expect(seen.filter((width) => width !== null)).toEqual([900]);
    expect(container.textContent).toBe("900");
  });

  it("renders a change inside the resize callback, in the same frame", () => {
    vi.stubGlobal("ResizeObserver", ManualObserver);
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(900);
    const { container } = render(<Probe />);
    act(() => {
      resizeTo(1000);
      // Rendered before the callback returns: nothing waits for a later frame.
      expect(container.textContent).toBe("1000");
    });
  });

  it("keeps the width when a resize reports the same size", () => {
    vi.stubGlobal("ResizeObserver", ManualObserver);
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(900);
    const { container } = render(<Probe />);
    act(() => resizeTo(900));
    expect(container.textContent).toBe("900");
    expect(seen.at(-1)).toBe(900);
  });
});
