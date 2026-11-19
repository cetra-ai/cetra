import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LazyMount } from "@/components/charts/lazy-mount";

type Callback = (entries: { isIntersecting: boolean; target: Element }[]) => void;

// Controllable IntersectionObserver stub: tests report intersections by hand.
class StubObserver {
  static instances: StubObserver[] = [];
  readonly targets = new Set<Element>();
  readonly disconnect = vi.fn(() => {
    this.targets.clear();
  });

  constructor(
    readonly callback: Callback,
    readonly options?: IntersectionObserverInit,
  ) {
    StubObserver.instances.push(this);
  }

  observe(target: Element) {
    this.targets.add(target);
  }

  unobserve(target: Element) {
    this.targets.delete(target);
  }

  takeRecords() {
    return [];
  }

  report(isIntersecting: boolean) {
    const entries = [...this.targets].map((target) => ({ isIntersecting, target }));
    this.callback(entries);
  }
}

const original = globalThis.IntersectionObserver;

beforeEach(() => {
  StubObserver.instances = [];
  globalThis.IntersectionObserver = StubObserver as unknown as typeof IntersectionObserver;
});

afterEach(() => {
  globalThis.IntersectionObserver = original;
});

function latest(): StubObserver {
  const observer = StubObserver.instances.at(-1);
  if (!observer) {
    throw new Error("no observer");
  }
  return observer;
}

describe("LazyMount", () => {
  it("renders children at once when eager", () => {
    render(
      <LazyMount eager minHeight={220} fallback={<p>card skeleton</p>}>
        <p>chart body</p>
      </LazyMount>,
    );
    expect(screen.getByText("chart body")).toBeInTheDocument();
    expect(screen.queryByText("card skeleton")).not.toBeInTheDocument();
    expect(StubObserver.instances).toHaveLength(0);
  });

  it("shows the fallback at the card height, then children once it intersects", () => {
    const { container } = render(
      <LazyMount eager={false} minHeight={220} fallback={<p>card skeleton</p>}>
        <p>chart body</p>
      </LazyMount>,
    );
    expect(screen.queryByText("chart body")).not.toBeInTheDocument();
    expect(screen.getByText("card skeleton")).toBeInTheDocument();
    expect(container.firstElementChild).toHaveStyle({ minHeight: "220px" });
    expect(latest().options).toMatchObject({ rootMargin: "400px 0px" });

    act(() => latest().report(false));
    expect(screen.queryByText("chart body")).not.toBeInTheDocument();

    act(() => latest().report(true));
    expect(screen.getByText("chart body")).toBeInTheDocument();
    expect(screen.queryByText("card skeleton")).not.toBeInTheDocument();
    expect(latest().disconnect).toHaveBeenCalled();
  });

  it("stays mounted after it leaves the viewport", () => {
    render(
      <LazyMount eager={false} minHeight={220} fallback={<p>card skeleton</p>}>
        <p>chart body</p>
      </LazyMount>,
    );
    act(() => latest().report(true));
    act(() => latest().report(false));
    expect(screen.getByText("chart body")).toBeInTheDocument();
  });

  it("disconnects the observer on unmount", () => {
    const { unmount } = render(
      <LazyMount eager={false} minHeight={220} fallback={<p>card skeleton</p>}>
        <p>chart body</p>
      </LazyMount>,
    );
    const observer = latest();
    unmount();
    expect(observer.disconnect).toHaveBeenCalled();
  });
});
