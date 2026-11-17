import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SplitPane } from "@/components/trace/split-pane";
import { setTraceMaximized } from "@/lib/list-layout";
import { renderApp } from "@/test/render";

const URL_WITH_TRACE = "/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000001";

function size(startPanelId: string): string | null {
  return (
    document
      .querySelector(`[role="separator"][aria-controls="${startPanelId}"]`)
      ?.getAttribute("aria-valuenow") ?? null
  );
}

function renderTracesSplit(mode?: "split" | "bottom") {
  render(
    <SplitPane
      name="traces"
      mode={mode}
      open
      start={<div>list body</div>}
      end={<div>trace body</div>}
    />,
  );
}

it("opens the trace view's inner split at its default ratio", async () => {
  await renderApp(URL_WITH_TRACE);
  await screen.findByRole("grid", { name: "Trace" });
  expect(size("trace-tree")).toBe("40");
});

it("opens the timeline's inner split at its own default ratio", async () => {
  await renderApp(`${URL_WITH_TRACE}&mode=timeline`);
  await screen.findByRole("grid", { name: "Trace" });
  expect(size("trace-timeline-tree")).toBe("60");
});

it("opens the split at its default ratio and restores a stored one", () => {
  renderTracesSplit();
  expect(size("traces-list")).toBe("40");
  cleanup();
  localStorage.setItem(
    "cetra.split.traces",
    JSON.stringify({ "traces-list": 30, "traces-detail": 70 }),
  );
  renderTracesSplit();
  expect(size("traces-list")).toBe("30");
});

it.each([
  ["not json", "{"],
  ["out of range", JSON.stringify({ "traces-list": 0, "traces-detail": 100 })],
  ["wrong keys", JSON.stringify({ list: 30, detail: 70 })],
])("falls back to the default for a stored value that is %s", (_label, raw) => {
  localStorage.setItem("cetra.split.traces", raw);
  renderTracesSplit();
  expect(size("traces-list")).toBe("40");
});

function separatorOrientation(startPanelId: string): string | null {
  return (
    document
      .querySelector(`[role="separator"][aria-controls="${startPanelId}"]`)
      ?.getAttribute("aria-orientation") ?? null
  );
}

it("reports the split separator orientation", () => {
  renderTracesSplit();
  expect(separatorOrientation("traces-list")).toBe("vertical");
});

it("reports the bottom separator orientation", async () => {
  renderTracesSplit("bottom");
  // Bottom slides the trace in first, then opens the layout. jsdom has no layout, so the sizes
  // that follow are not observable here.
  await waitFor(() => {
    expect(separatorOrientation("traces-list")).toBe("horizontal");
  });
});

it("drawer mode keeps the list and shows no trace panel when closed", () => {
  render(
    <SplitPane
      name="traces"
      mode="drawer"
      open={false}
      start={<div>list body</div>}
      end={<div>trace body</div>}
    />,
  );
  expect(screen.getByText("list body")).toBeInTheDocument();
  expect(screen.queryByRole("complementary", { name: "Trace" })).not.toBeInTheDocument();
  expect(screen.queryByText("trace body")).not.toBeInTheDocument();
});

it("drawer mode shows the trace as a non-modal panel over the list", () => {
  render(
    <SplitPane
      name="traces"
      mode="drawer"
      open
      start={<div>list body</div>}
      end={<div>trace body</div>}
    />,
  );
  const drawer = screen.getByRole("complementary", { name: "Trace" });
  expect(drawer).toContainElement(screen.getByText("trace body"));
  expect(screen.getByText("list body")).toBeInTheDocument();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(drawer).toHaveClass("drawer-in");
});

describe("drawer resize", () => {
  afterEach(() => {
    localStorage.clear();
  });

  function renderDrawer() {
    render(
      <SplitPane
        name="traces"
        mode="drawer"
        open
        start={<div>list body</div>}
        end={<div>trace body</div>}
      />,
    );
    return screen.getByRole("separator", { name: "Resize trace" });
  }

  it("starts at 70% and has a focusable handle", () => {
    const handle = renderDrawer();
    expect(handle).toHaveAttribute("aria-valuenow", "70");
    expect(screen.getByRole("complementary", { name: "Trace" })).toHaveStyle({ width: "70%" });
  });

  it("arrow keys resize and store the width", () => {
    const handle = renderDrawer();
    fireEvent.keyDown(handle, { key: "ArrowLeft" });
    expect(handle).toHaveAttribute("aria-valuenow", "75");
    expect(localStorage.getItem("cetra.split.traces.drawer")).toBe("75");
    fireEvent.keyDown(handle, { key: "ArrowRight" });
    fireEvent.keyDown(handle, { key: "ArrowRight" });
    expect(handle).toHaveAttribute("aria-valuenow", "65");
  });

  it("clamps to 30..95 percent", () => {
    const handle = renderDrawer();
    for (let i = 0; i < 20; i += 1) {
      fireEvent.keyDown(handle, { key: "ArrowRight" });
    }
    expect(handle).toHaveAttribute("aria-valuenow", "30");
    for (let i = 0; i < 20; i += 1) {
      fireEvent.keyDown(handle, { key: "ArrowLeft" });
    }
    expect(handle).toHaveAttribute("aria-valuenow", "95");
  });

  it("restores the stored width and ignores an invalid one", () => {
    localStorage.setItem("cetra.split.traces.drawer", "50");
    expect(renderDrawer()).toHaveAttribute("aria-valuenow", "50");
  });

  it("falls back to 70% for an invalid stored width", () => {
    localStorage.setItem("cetra.split.traces.drawer", "5");
    expect(renderDrawer()).toHaveAttribute("aria-valuenow", "70");
  });
});

describe("page mode", () => {
  it("shows the list alone when no trace is open", () => {
    render(
      <SplitPane
        name="traces"
        mode="page"
        open={false}
        start={<div>list body</div>}
        end={<div>trace body</div>}
        filmstrip={<div>strip</div>}
      />,
    );
    expect(screen.getByText("list body").parentElement).not.toHaveClass("hidden");
    expect(screen.queryByText("trace body")).not.toBeInTheDocument();
  });

  it("shows the filmstrip above the trace and hides the list", () => {
    render(
      <SplitPane
        name="traces"
        mode="page"
        open
        start={<div>list body</div>}
        end={<div>trace body</div>}
        filmstrip={<div>strip</div>}
      />,
    );
    expect(screen.getByText("strip")).toBeInTheDocument();
    expect(screen.getByText("trace body")).toBeInTheDocument();
    expect(screen.getByText("list body").parentElement).toHaveClass("hidden");
  });
});

describe("maximized trace", () => {
  afterEach(() => {
    setTraceMaximized(false);
  });

  it("fills the area over the layout and renders the trace once", () => {
    setTraceMaximized(true);
    render(
      <SplitPane
        name="traces"
        mode="split"
        open
        start={<div>list body</div>}
        end={<div>trace body</div>}
      />,
    );
    expect(screen.getAllByText("trace body")).toHaveLength(1);
    expect(screen.getByRole("region", { name: "Trace" })).toContainElement(
      screen.getByText("trace body"),
    );
  });

  it("is reset when the trace closes", () => {
    setTraceMaximized(true);
    const { rerender } = render(
      <SplitPane name="traces" open start={<div>l</div>} end={<div>t</div>} />,
    );
    rerender(<SplitPane name="traces" open={false} start={<div>l</div>} end={<div>t</div>} />);
    expect(screen.queryByRole("region", { name: "Trace" })).not.toBeInTheDocument();
    rerender(<SplitPane name="traces" open start={<div>l</div>} end={<div>t</div>} />);
    expect(screen.queryByRole("region", { name: "Trace" })).not.toBeInTheDocument();
  });
});

describe("bottom slide", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function bottom(open: boolean, end = "trace body") {
    return (
      <SplitPane
        name="traces"
        mode="bottom"
        open={open}
        start={<div>list body</div>}
        end={open ? <div>{end}</div> : null}
      />
    );
  }

  it("slides the trace up over the list, then opens the layout", () => {
    const { rerender } = render(bottom(false));
    rerender(bottom(true));
    expect(screen.getByText("trace body").parentElement).toHaveClass("slide-start");
    expect(document.querySelector('[role="separator"]')).toBeNull();
    act(() => {
      vi.advanceTimersByTime(50);
    });
    expect(screen.getByText("trace body").parentElement).toHaveClass("slide-up-in");
    act(() => {
      vi.advanceTimersByTime(400);
    });
    expect(screen.getByText("trace body").parentElement).not.toHaveClass("slide-up-in");
    expect(document.querySelector('[role="separator"]')).not.toBeNull();
  });

  it("gives the list its space back, then slides the last trace down and unmounts it", () => {
    const { rerender } = render(bottom(true));
    rerender(bottom(false));
    expect(screen.getByText("trace body").parentElement).toHaveClass("slide-down-out");
    expect(document.querySelector('[role="separator"]')).toBeNull();
    act(() => {
      vi.advanceTimersByTime(400);
    });
    expect(screen.queryByText("trace body")).not.toBeInTheDocument();
  });

  it("slides the trace in when it mounts open, as after a layout switch", () => {
    render(bottom(true));
    expect(screen.getByText("trace body").parentElement).toHaveClass("slide-start");
    act(() => {
      vi.advanceTimersByTime(50);
    });
    expect(screen.getByText("trace body").parentElement).toHaveClass("slide-up-in");
  });

  it("does not slide in split mode", () => {
    const { rerender } = render(
      <SplitPane name="traces" open={false} start={<div>l</div>} end={<div>trace body</div>} />,
    );
    rerender(<SplitPane name="traces" open start={<div>l</div>} end={<div>trace body</div>} />);
    expect(screen.getByText("trace body").parentElement).not.toHaveClass("slide-up-in");
  });
});

describe("drawer exit", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function drawer(open: boolean, end: string) {
    return (
      <SplitPane
        name="traces"
        mode="drawer"
        open={open}
        start={<div>list body</div>}
        end={open ? <div>{end}</div> : null}
      />
    );
  }

  it("slides out with the last trace, then unmounts", () => {
    const { rerender } = render(drawer(true, "trace body"));
    rerender(drawer(false, "trace body"));
    const aside = screen.getByRole("complementary", { name: "Trace" });
    expect(aside).toHaveClass("drawer-out");
    expect(aside).toContainElement(screen.getByText("trace body"));
    act(() => {
      vi.advanceTimersByTime(400);
    });
    expect(screen.queryByRole("complementary", { name: "Trace" })).not.toBeInTheDocument();
  });

  it("slides back in when reopened during the exit", () => {
    const { rerender } = render(drawer(true, "one"));
    rerender(drawer(false, "one"));
    rerender(drawer(true, "two"));
    const aside = screen.getByRole("complementary", { name: "Trace" });
    expect(aside).toHaveClass("drawer-in");
    expect(aside).toContainElement(screen.getByText("two"));
  });
});

describe("page fade", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function page(open: boolean) {
    return (
      <SplitPane
        name="traces"
        mode="page"
        open={open}
        start={<div>list body</div>}
        end={open ? <div>trace body</div> : null}
        filmstrip={open ? <div>strip</div> : null}
      />
    );
  }

  it("fades the page in when a trace opens", () => {
    const { rerender } = render(page(false));
    rerender(page(true));
    expect(screen.getByText("strip").parentElement).toHaveClass("page-in");
  });

  it("shows the list at once on close and fades the last page out over it", () => {
    const { rerender } = render(page(true));
    rerender(page(false));
    expect(screen.getByText("list body").parentElement).not.toHaveClass("hidden");
    expect(screen.getByText("trace body")).toBeInTheDocument();
    expect(screen.getByText("strip").parentElement).toHaveClass("page-out");
    act(() => {
      vi.advanceTimersByTime(400);
    });
    expect(screen.queryByText("trace body")).not.toBeInTheDocument();
  });
});

describe("switching layouts with a trace open", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function pane(mode: "drawer" | "bottom" | "page", open: boolean) {
    return (
      <SplitPane
        name="traces"
        mode={mode}
        open={open}
        start={<div>list body</div>}
        end={open ? <div>trace body</div> : null}
        filmstrip={open ? <div>strip</div> : null}
      />
    );
  }

  it("closes the current layout, then opens the new one", () => {
    const { rerender } = render(pane("drawer", true));
    rerender(pane("bottom", true));
    // Phase 1: the drawer slides out with the trace it had.
    const aside = screen.getByRole("complementary", { name: "Trace" });
    expect(aside).toHaveClass("drawer-out");
    expect(aside).toContainElement(screen.getByText("trace body"));
    expect(document.querySelector("[data-fast]")).not.toBeNull();
    act(() => {
      vi.advanceTimersByTime(200);
    });
    // Phase 2: the bottom layout is mounted and slides the trace in.
    expect(screen.queryByRole("complementary", { name: "Trace" })).not.toBeInTheDocument();
    expect(screen.getByText("trace body").parentElement).toHaveClass("slide-start");
    act(() => {
      vi.advanceTimersByTime(400);
    });
    expect(document.querySelector("[data-fast]")).toBeNull();
  });

  it("switches at once when no trace is open", () => {
    const { rerender } = render(pane("drawer", false));
    rerender(pane("page", false));
    expect(document.querySelector("[data-fast]")).toBeNull();
    expect(screen.getByText("list body")).toBeInTheDocument();
  });
});
