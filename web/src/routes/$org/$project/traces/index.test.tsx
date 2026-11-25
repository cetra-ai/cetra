import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { beforeAll, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { columnChoiceKey } from "@/components/data-table/column-layout";
import { ROW_HEIGHT } from "@/components/data-table/data-table";
import { LIVE_BUFFER_MAX, LIVE_PAGE_MAX } from "@/components/data-table/live-list";
import { traceProbeOptions } from "@/components/data-table/use-infinite-list";
import { setLiveLimits } from "@/components/data-table/use-live-traces";
import { ONBOARDING_POLL_MS, setOnboardingPollMs } from "@/components/onboarding/poll";
import { otelSnippet } from "@/components/onboarding/snippets";
import { defaultCollapsed } from "@/lib/tree";
import { createDataset, type Dataset } from "@/mocks/generated/dataset";
import { largeHandlers } from "@/mocks/generated/handlers";
import { server } from "@/mocks/node";
import { savedViewHandlers, seedWithDefaultView } from "@/mocks/saved-views";
import { type StreamEmit, spanStreamHandler, traceStreamHandler } from "@/mocks/sse";
import { facetsHandler } from "@/test/facets";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { recordUrls } from "@/test/requests";
import tracesFixture from "../../../../../../fixtures/api/responses/traces.json";
import queuesFixture from "../../../../../../fixtures/api/static/annotation-queues.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const FIRST = "a1000000000000000000000000000006";

function slotRect(left: number, width: number): DOMRect {
  return {
    x: left,
    y: 0,
    left,
    right: left + width,
    top: 0,
    bottom: 28,
    width,
    height: 28,
    toJSON: () => ({}),
  };
}

// Each header is 100 px wide, so the drop index follows the column order.
function stubHeaderSlots(grid: HTMLElement) {
  // The selection column has no column id and is not a sortable column: the slots count the columns only.
  within(grid)
    .getAllByRole("columnheader")
    .filter((header) => header.hasAttribute("data-column-id"))
    .forEach((header, index) => {
      header.getBoundingClientRect = () => slotRect(index * 100, 100);
    });
  const scroller = grid.querySelector<HTMLElement>("[data-scroll-container]");
  if (scroller) {
    scroller.getBoundingClientRect = () => slotRect(0, 1000);
  }
}

it("saves a column dragged to a new place as cols", async () => {
  const { router } = await renderApp("/acme/support-bot/traces");
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Traces"]);
  const grid = await screen.findByRole("grid", { name: "Traces" });
  stubHeaderSlots(grid);
  vi.useFakeTimers();
  try {
    // Input is the second unlocked column; drop it on the Start slot.
    fireEvent.pointerDown(screen.getByRole("columnheader", { name: "Input" }), {
      button: 0,
      clientX: 350,
      clientY: 10,
    });
    fireEvent.pointerMove(window, { clientX: 120, clientY: 10 });
    fireEvent.pointerUp(window, { clientX: 120, clientY: 10 });
  } finally {
    vi.useRealTimers();
  }
  await waitFor(() => {
    expect(
      String(router.state.location.search.cols ?? "")
        .split(",")
        .slice(0, 4),
    ).toEqual(["status", "input", "start", "name"]);
  });
});

it("renders the fixture traces in the default columns", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    await renderApp("/acme/support-bot/traces");
    const grid = await screen.findByRole("grid", { name: "Traces" });
    expect(grid).toHaveTextContent("chat claude-sonnet-4-5");
    expect(screen.getByText("5 loaded")).toBeInTheDocument();
    expect(screen.queryByText("judge helpfulness")).not.toBeInTheDocument();
    for (const name of [
      "Start",
      "Name",
      "Input",
      "Output",
      "Duration",
      "Tokens",
      "Cost",
      "Models",
      "Session",
      "User",
      "Tags",
      "Env",
    ]) {
      expect(screen.getByRole("columnheader", { name })).toBeInTheDocument();
    }
    expect(screen.getByRole("columnheader", { name: "Status" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Start" })).toHaveAttribute(
      "aria-sort",
      "descending",
    );
    expect(screen.getByText("Summarize the open ticket.")).toBeInTheDocument();
    expect(screen.getByText("2.00s")).toBeInTheDocument();
    expect(screen.getByText("$0.00306")).toBeInTheDocument();
    expect(screen.getAllByText("beta").length).toBeGreaterThan(0);
    expect(screen.getByText("refund")).toBeInTheDocument();
    expect(urls[0]?.searchParams.get("limit")).toBe("100");
    expect(urls[0]?.searchParams.get("from")).toBeTruthy();
    expect(urls[0]?.searchParams.has("sort")).toBe(false);
    expect(urls[0]?.searchParams.has("cursor")).toBe(false);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("renders the traces-over-time chart above the table", async () => {
  await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  expect(screen.getByRole("img", { name: "Traces over time" })).toBeInTheDocument();
});

it("sorts from the URL without reordering the server page", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    await renderApp("/acme/support-bot/traces?sort=duration&order=asc");
    expect(await screen.findByRole("columnheader", { name: "Duration" })).toHaveAttribute(
      "aria-sort",
      "ascending",
    );
    expect(screen.getByRole("columnheader", { name: "Start" })).toHaveAttribute(
      "aria-sort",
      "none",
    );
    await waitFor(() => {
      expect(urls.some((url) => url.searchParams.get("sort") === "duration")).toBe(true);
    });
    expect(
      urls.find((url) => url.searchParams.get("sort") === "duration")?.searchParams.get("order"),
    ).toBe("asc");
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

let traces5k: Dataset;

beforeAll(() => {
  traces5k = createDataset({ seed: 1, now: Date.now(), traces: 5000 });
});

it("virtualizes a large list and loads the next page on scroll", async () => {
  server.use(...largeHandlers(traces5k));
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    await renderApp("/acme/support-bot/traces");
    await screen.findByRole("grid", { name: "Traces" });
    await waitFor(() => {
      const count = screen.getAllByRole("row").length;
      expect(count).toBeGreaterThan(1);
      expect(count).toBeLessThan(60);
    });
    const scroller = document.querySelector("[data-scroll-container]");
    if (!(scroller instanceof HTMLElement)) {
      throw new Error("missing scroll container");
    }
    scroller.scrollTop = scroller.scrollHeight;
    fireEvent.scroll(scroller);
    await waitFor(() => {
      expect(urls.some((url) => url.searchParams.has("cursor"))).toBe(true);
    });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("moves with j and k and activates the focused row", async () => {
  const { router } = await renderApp("/acme/support-bot/traces");
  const grid = await screen.findByRole("grid", { name: "Traces" });
  grid.focus();
  fireEvent.keyDown(grid, { key: "j" });
  fireEvent.keyDown(grid, { key: "j" });
  fireEvent.keyDown(grid, { key: "k" });
  fireEvent.keyDown(grid, { key: "Enter" });
  expect(router.state.location.search).toMatchObject({ trace: FIRST });
  expect(await within(grid).findByRole("row", { selected: true })).toHaveTextContent(
    "chat claude-sonnet-4-5",
  );
});

it("scrolls the header with the rows", async () => {
  await renderApp("/acme/support-bot/traces?range=1h");
  const grid = await screen.findByRole("grid", { name: "Traces" });
  const scroller = grid.querySelector("[data-scroll-container]");
  if (!(scroller instanceof HTMLElement)) {
    throw new Error("missing scroll container");
  }
  const [header, firstRow] = within(grid).getAllByRole("row");
  if (!header || !firstRow) {
    throw new Error("missing rows");
  }
  expect(header.style.minWidth).toMatch(/^\d+px$/);
  expect(firstRow.style.minWidth).toBe(header.style.minWidth);
  scroller.scrollLeft = 300;
  fireEvent.scroll(scroller);
  expect(header.parentElement?.scrollLeft).toBe(300);
});

it("moves from the open trace after a deep link", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000003",
  );
  const list = await screen.findByRole("grid", { name: "Traces" });
  await screen.findByRole("grid", { name: "Trace" });
  list.focus();
  fireEvent.keyDown(list, { key: "j" });
  await waitFor(() => {
    expect(router.state.location.search.trace).toBe("a1000000000000000000000000000002");
  });
  fireEvent.keyDown(list, { key: "k" });
  await waitFor(() => {
    expect(router.state.location.search.trace).toBe("a1000000000000000000000000000003");
  });
});

it("moves up from the open trace after a deep link", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000003",
  );
  const list = await screen.findByRole("grid", { name: "Traces" });
  await screen.findByRole("grid", { name: "Trace" });
  list.focus();
  fireEvent.keyDown(list, { key: "k" });
  await waitFor(() => {
    expect(router.state.location.search.trace).toBe("a1000000000000000000000000000004");
  });
});

it("opens the neighbouring loaded traces from the header buttons", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000003",
  );
  await screen.findByRole("grid", { name: "Trace" });
  fireEvent.click(screen.getByRole("button", { name: "Next trace" }));
  await waitFor(() => {
    expect(router.state.location.search.trace).toBe("a1000000000000000000000000000002");
  });
  fireEvent.click(screen.getByRole("button", { name: "Previous trace" }));
  await waitFor(() => {
    expect(router.state.location.search.trace).toBe("a1000000000000000000000000000003");
  });
});

it("hides Next at the last loaded trace", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000003",
  );
  await screen.findByRole("grid", { name: "Trace" });
  let steps = 0;
  while (screen.queryByRole("button", { name: "Next trace" }) && steps < 50) {
    const before = router.state.location.search.trace;
    fireEvent.click(screen.getByRole("button", { name: "Next trace" }));
    await waitFor(() => {
      expect(router.state.location.search.trace).not.toBe(before);
    });
    steps += 1;
  }
  expect(steps).toBeGreaterThan(0);
  expect(screen.queryByRole("button", { name: "Next trace" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Previous trace" })).toBeInTheDocument();
});

it("persists a column width and restores the default on double-click", async () => {
  await renderApp("/acme/support-bot/traces");
  const handle = await screen.findByRole("separator", { name: "Resize Name" });
  fireEvent.pointerDown(handle, { clientX: 100 });
  fireEvent.pointerMove(window, { clientX: 180 });
  fireEvent.pointerUp(window, { clientX: 180 });
  expect(JSON.parse(localStorage.getItem("cetra.table.traces") ?? "")).toEqual({ name: 280 });
  const header = handle.parentElement?.parentElement;
  expect(header?.style.gridTemplateColumns.startsWith("32px 28px 132px 280px")).toBe(true);
  fireEvent.doubleClick(handle);
  expect(localStorage.getItem("cetra.table.traces")).toBeNull();
  expect(header?.style.gridTemplateColumns.startsWith("32px 28px 132px 200px")).toBe(true);
});

const TRACE_1 = "a1000000000000000000000000000001";
const TOOL_SPAN = "b100000000000013";
const NEXT = "a1000000000000000000000000000004";
const NEXT_OF_3 = "a1000000000000000000000000000002";
const PREVIOUS_OF_3 = "a1000000000000000000000000000004";
const LARGE = "a0000000000000000000000000000000";

it("opens a trace beside the list and follows span, escape, and j", async () => {
  const { router } = await renderApp("/acme/support-bot/traces");
  const list = await screen.findByRole("grid", { name: "Traces" });
  const scroller = list.querySelector("[data-scroll-container]");
  if (!(scroller instanceof HTMLElement)) {
    throw new Error("missing scroll container");
  }
  scroller.scrollTop = 240;
  fireEvent.scroll(scroller);

  fireEvent.click(within(list).getByRole("row", { name: /4\.20s/ }));
  const tree = await screen.findByRole("grid", { name: "Trace" });
  expect(router.state.location.search).toMatchObject({ trace: TRACE_1 });
  const dataRows = within(tree)
    .getAllByRole("row")
    .filter((row) => row.getAttribute("aria-rowindex") !== "1");
  expect(dataRows).toHaveLength(4);
  expect(within(tree).getByRole("row", { selected: true })).toHaveTextContent("support-agent");
  expect(await screen.findByRole("tabpanel")).toHaveTextContent("My order 1234 hasn't arrived.");
  const listAfter = screen.getByRole("grid", { name: "Traces" });
  const scrollerAfter = listAfter.querySelector("[data-scroll-container]");
  if (!(scrollerAfter instanceof HTMLElement)) {
    throw new Error("missing scroll container");
  }
  expect(scrollerAfter.scrollTop).toBe(240);

  fireEvent.click(within(tree).getByRole("row", { name: /get_order_status/ }));
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({ trace: TRACE_1, span: TOOL_SPAN });
  });
  expect(await screen.findByRole("tabpanel")).toHaveTextContent("order_id");

  fireEvent.keyDown(window, { key: "Escape" });
  await waitFor(() => {
    expect(router.state.location.search.trace).toBeUndefined();
  });
  expect(router.state.location.search.span).toBeUndefined();
  // The drawer slides out before it unmounts.
  await waitFor(() => {
    expect(screen.queryByRole("grid", { name: "Trace" })).not.toBeInTheDocument();
  });
  expect(screen.getByRole("grid", { name: "Traces" })).toBeInTheDocument();

  const firstRow = within(list).getAllByRole("row")[1];
  if (!firstRow) {
    throw new Error("missing trace row");
  }
  fireEvent.click(firstRow);
  await screen.findByRole("grid", { name: "Trace" });
  list.focus();
  fireEvent.keyDown(list, { key: "j" });
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({ trace: NEXT });
  });
  expect(router.state.location.search.span).toBeUndefined();
});

it("shows the trace summary in transcript mode until a block is picked, and goes back to it", async () => {
  const { router, user } = await renderApp(
    `/acme/support-bot/traces?trace=${TRACE_1}&mode=transcript`,
  );
  expect(await screen.findByRole("region", { name: "Trace summary" })).toBeInTheDocument();

  const transcript = await screen.findByRole("region", { name: "Transcript" });
  await user.click(within(transcript).getByRole("button", { name: /get_order_status/ }));
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({ span: TOOL_SPAN, mode: "transcript" });
  });
  expect(screen.queryByRole("region", { name: "Trace summary" })).not.toBeInTheDocument();
  const panel = await screen.findByRole("region", { name: "Span" });
  expect(within(panel).getByText("get_order_status")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Trace summary" }));
  await waitFor(() => {
    expect(router.state.location.search.span).toBeUndefined();
  });
  expect(router.state.location.search).toMatchObject({ trace: TRACE_1, mode: "transcript" });
  expect(await screen.findByRole("region", { name: "Trace summary" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Span" })).not.toBeInTheDocument();
});

it("shows the root span's detail in tree mode, not the summary", async () => {
  await renderApp(`/acme/support-bot/traces?trace=${TRACE_1}`);
  const tree = await screen.findByRole("grid", { name: "Trace" });
  expect(within(tree).getByRole("row", { selected: true })).toHaveTextContent("support-agent");
  expect(await screen.findByRole("region", { name: "Span" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Trace summary" })).not.toBeInTheDocument();
});

it("virtualizes a 10 000 span trace and reveals children when a node expands", async () => {
  const dataset = createDataset({ seed: 1, now: Date.now(), traces: 1 });
  server.use(...largeHandlers(dataset));
  await renderApp(`/acme/support-bot/traces?trace=${LARGE}`);
  const tree = await screen.findByRole("grid", { name: "Trace" });
  await waitFor(() => {
    const count = within(tree).getAllByRole("row").length;
    expect(count).toBeGreaterThan(1);
    expect(count).toBeLessThan(80);
  });
  const detail = dataset.traceDetail(LARGE);
  expect(detail).toBeDefined();
  expect(defaultCollapsed(detail?.spans ?? []).size).toBeGreaterThan(0);

  const before = within(tree)
    .getAllByRole("row")
    .map((row) => row.textContent);
  const expand = within(tree).getAllByRole("button", { name: "Expand" })[0];
  if (!expand) {
    throw new Error("missing collapsed span");
  }
  fireEvent.click(expand);
  await waitFor(() => {
    const after = within(tree)
      .getAllByRole("row")
      .map((row) => row.textContent);
    expect(after.some((text) => !before.includes(text))).toBe(true);
  });
});

it("chooses a preset and refreshes against a new anchor", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { router } = await renderApp("/acme/support-bot/traces");
    await screen.findByRole("grid", { name: "Traces" });
    fireEvent.click(screen.getByRole("button", { name: "Last 24 hours" }));
    fireEvent.click(await screen.findByRole("button", { name: "Last 1 hour" }));
    await waitFor(() => {
      expect(urls.some((url) => near(url, 3_600_000))).toBe(true);
    });
    expect(router.state.location.search).toMatchObject({ range: "1h" });
    const before = urls
      .filter((url) => near(url, 3_600_000))
      .at(-1)
      ?.searchParams.get("from");
    await new Promise((resolve) => setTimeout(resolve, 30));
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => {
      const after = urls
        .filter((url) => near(url, 3_600_000))
        .at(-1)
        ?.searchParams.get("from");
      expect(after).toBeTruthy();
      expect(after).not.toBe(before);
    });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("sends a search after the debounce", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { router } = await renderApp("/acme/support-bot/traces");
    const input = await screen.findByRole("searchbox", { name: "Search" });
    fireEvent.change(input, { target: { value: "refund" } });
    await waitFor(() => {
      expect(urls.some((url) => url.searchParams.get("search") === "refund")).toBe(true);
    });
    expect(router.state.location.search).toMatchObject({ search: "refund" });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("includes evaluation traces when eval is on", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { router, user } = await renderApp("/acme/support-bot/traces");
    await screen.findByRole("grid", { name: "Traces" });
    expect(screen.queryByText("judge helpfulness")).not.toBeInTheDocument();
    await user.click(
      screen.getByRole("button", { name: /^Include evaluation and experiment traces/ }),
    );
    await user.click(await screen.findByRole("menuitemcheckbox", { name: "Evaluation traces" }));
    expect(await screen.findByText("judge helpfulness")).toBeInTheDocument();
    expect(urls.some((url) => url.searchParams.get("include_eval") === "true")).toBe(true);
    expect(router.state.location.search).toMatchObject({ eval: 1 });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("shows score columns after cost", async () => {
  await renderApp("/acme/support-bot/traces");
  const grid = await screen.findByRole("grid", { name: "Traces" });
  const headers = within(grid)
    .getAllByRole("columnheader")
    .map((header) => header.textContent);
  const cost = headers.indexOf("Cost");
  expect(headers.slice(cost, cost + 4)).toEqual(["Cost", "helpfulness", "note", "thumbs"]);
  expect(within(grid).getByText("0.90")).toBeInTheDocument();
  expect(within(grid).getByText("✓")).toBeInTheDocument();
  expect(within(grid).getByText("Needs retry with backoff.")).toBeInTheDocument();
  expect(within(grid).queryByRole("columnheader", { name: "Release" })).not.toBeInTheDocument();
});

it("restores column order from cols and ignores an unknown id", async () => {
  await renderApp("/acme/support-bot/traces?cols=nope,user,name,status");
  const grid = await screen.findByRole("grid", { name: "Traces" });
  expect(
    within(grid)
      .getAllByRole("columnheader")
      .filter((header) => header.hasAttribute("data-column-id"))
      .map((header) => header.textContent),
  ).toEqual(["User", "Name", "Status"]);
});

it("toggles a column from the chooser", async () => {
  const { user, router } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await user.click(screen.getByRole("button", { name: "Columns" }));
  expect(await screen.findByRole("checkbox", { name: "Status" })).toBeDisabled();
  expect(screen.getByRole("checkbox", { name: "Name" })).toBeDisabled();
  await user.click(screen.getByRole("checkbox", { name: "Models" }));
  await waitFor(() => {
    expect(screen.queryByRole("columnheader", { name: "Models" })).not.toBeInTheDocument();
  });
  const cols = String(router.state.location.search.cols ?? "").split(",");
  expect(cols).not.toContain("models");
  expect(cols).toContain("score:helpfulness");
  expect(cols).toContain("status");
});

it("sorts by cost, then ascending, then back to the default", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { router } = await renderApp("/acme/support-bot/traces");
    const header = await screen.findByRole("columnheader", { name: "Cost" });
    const button = within(header).getByRole("button", { name: "Cost" });
    fireEvent.click(button);
    await waitFor(() => {
      expect(urls.some((url) => url.searchParams.get("sort") === "cost_total")).toBe(true);
      expect(screen.getByRole("columnheader", { name: "Cost" })).toHaveAttribute(
        "aria-sort",
        "descending",
      );
    });
    expect(
      urls.find((url) => url.searchParams.get("sort") === "cost_total")?.searchParams.get("order"),
    ).toBe("desc");
    expect(router.state.location.search).toMatchObject({ sort: "cost_total", order: "desc" });
    fireEvent.click(
      within(screen.getByRole("columnheader", { name: "Cost" })).getByRole("button", {
        name: "Cost",
      }),
    );
    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({ sort: "cost_total", order: "asc" });
      expect(screen.getByRole("columnheader", { name: "Cost" })).toHaveAttribute(
        "aria-sort",
        "ascending",
      );
    });
    fireEvent.click(
      within(screen.getByRole("columnheader", { name: "Cost" })).getByRole("button", {
        name: "Cost",
      }),
    );
    await waitFor(() => {
      expect(router.state.location.search.sort).toBeUndefined();
      expect(router.state.location.search.order).toBeUndefined();
      expect(screen.getByRole("columnheader", { name: "Start" })).toHaveAttribute(
        "aria-sort",
        "descending",
      );
    });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("uses the stored columns when the URL has no cols", async () => {
  localStorage.setItem(columnChoiceKey("traces"), "user,name,status");
  await renderApp("/acme/support-bot/traces");
  const grid = await screen.findByRole("grid", { name: "Traces" });
  expect(
    within(grid)
      .getAllByRole("columnheader")
      .filter((header) => header.hasAttribute("data-column-id"))
      .map((header) => header.textContent),
  ).toEqual(["User", "Name", "Status"]);
});

function near(url: URL, windowMs: number): boolean {
  const from = Date.parse(url.searchParams.get("from") ?? "");
  return Math.abs(from - (Date.now() - windowMs)) < 15_000;
}

it("shows the plain empty state when the default filter matches nothing", async () => {
  savedViewHandlers(seedWithDefaultView());
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByRole("heading", { name: "No traces match" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Clear filters" })).toBeInTheDocument();
  expect(screen.queryByRole("tab", { name: "OTel env vars" })).not.toBeInTheDocument();
  expect(screen.getByText("0 loaded")).toBeInTheDocument();
  expect(screen.queryByRole("status", { name: "Totals" })).not.toBeInTheDocument();
});

it("the saved views menu sits in the breadcrumb", async () => {
  savedViewHandlers([]);
  await renderApp("/acme/support-bot/traces");
  const crumb = await screen.findByRole("navigation", { name: "Breadcrumb" });
  expect(await within(crumb).findByRole("button", { name: "Default view" })).toBeInTheDocument();
});

it("shows onboarding snippets when the project has no traces", async () => {
  savedViewHandlers([]);
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByRole("tab", { name: "OTel env vars" })).toBeInTheDocument();
  const endpoint = `${window.location.origin}/v1/traces`;
  expect(screen.getByText(endpoint, { exact: false })).toBeInTheDocument();
  expect(screen.getByText("Waiting for the first trace…")).toBeInTheDocument();
  expect(screen.getByText("Create a secret key in Settings → API keys")).toBeInTheDocument();
  const writeText = stubClipboard();
  fireEvent.click(screen.getByRole("button", { name: "Copy OTel env vars" }));
  await waitFor(() => {
    expect(writeText).toHaveBeenCalledWith(otelSnippet(window.location.origin));
  });
});

it("links admins to the API keys page from onboarding", async () => {
  savedViewHandlers([]);
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  await renderApp("/acme/support-bot/traces");
  expect(
    await screen.findByRole("link", { name: "Create a secret key in Settings → API keys" }),
  ).toHaveAttribute("href", "/acme/support-bot/settings/api-keys");
});

it("asks members to get a key from an admin", async () => {
  savedViewHandlers([]);
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "member" }),
    ),
  );
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByText("Ask a project admin for a secret key")).toBeInTheDocument();
  expect(screen.queryByRole("link", { name: /API keys/ })).toBeNull();
});

it("clears a filter and shows onboarding when nothing is left", async () => {
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  const { router, user } = await renderApp(
    "/acme/support-bot/traces?filter=name%20%3D%20%27missing%27",
  );
  expect(await screen.findByRole("heading", { name: "No traces match" })).toBeInTheDocument();
  expect(screen.queryByRole("tab", { name: "OTel env vars" })).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Clear filters" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBeUndefined();
  });
  expect(router.state.location.search).toEqual({});
  expect(await screen.findByRole("tab", { name: "OTel env vars" })).toBeInTheDocument();
});

it("replaces onboarding with the table when a trace arrives", async () => {
  savedViewHandlers([]);
  setOnboardingPollMs(20);
  let ready = false;
  const dataset = createDataset({ seed: 1, now: Date.now(), traces: 1 });
  const trace = dataset.traces[0];
  if (!trace) {
    throw new Error("missing trace");
  }
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: ready ? [trace] : [], next_cursor: null }),
    ),
  );
  try {
    await renderApp("/acme/support-bot/traces");
    expect(await screen.findByText("Waiting for the first trace…")).toBeInTheDocument();
    ready = true;
    expect(await screen.findByRole("grid", { name: "Traces" })).toHaveTextContent(trace.name);
    expect(screen.queryByText("Waiting for the first trace…")).not.toBeInTheDocument();
  } finally {
    setOnboardingPollMs(ONBOARDING_POLL_MS);
  }
});

it("shows onboarding when the all-time probe fails", async () => {
  savedViewHandlers([]);
  server.use(
    http.get("/api/v1/projects/:project/traces", ({ request }) => {
      const url = new URL(request.url);
      if (!url.searchParams.has("from")) {
        return problemResponse(500, "internal", { detail: "probe failed" });
      }
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByRole("tab", { name: "OTel env vars" })).toBeInTheDocument();
  expect(screen.queryByText("No traces in this time range.")).not.toBeInTheDocument();
});

it("shows neither the empty range nor onboarding while the probe loads", async () => {
  savedViewHandlers([]);
  let release: (() => void) | undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.get("/api/v1/projects/:project/traces", async ({ request }) => {
      const url = new URL(request.url);
      if (!url.searchParams.has("from")) {
        await gate;
      }
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );
  try {
    await renderApp("/acme/support-bot/traces");
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(screen.queryByText("No traces in this time range.")).not.toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "OTel env vars" })).not.toBeInTheDocument();
    release?.();
    expect(await screen.findByRole("tab", { name: "OTel env vars" })).toBeInTheDocument();
  } finally {
    release?.();
  }
});

it("hides onboarding while a cached empty probe refetches", async () => {
  savedViewHandlers([]);
  let probes = 0;
  let release: (() => void) | undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.get("/api/v1/projects/:project/traces", async ({ request }) => {
      if (!new URL(request.url).searchParams.has("from")) {
        probes += 1;
        if (probes > 1) {
          await gate;
        }
      }
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );
  try {
    const { queryClient } = await renderApp("/acme/support-bot/traces");
    expect(await screen.findByRole("tab", { name: "OTel env vars" })).toBeInTheDocument();
    void queryClient.invalidateQueries({
      queryKey: traceProbeOptions("acme", "support-bot").queryKey,
    });
    await waitFor(() => expect(probes).toBe(2));
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(screen.queryByRole("tab", { name: "OTel env vars" })).not.toBeInTheDocument();
    expect(screen.queryByText("No traces in this time range.")).not.toBeInTheDocument();
    release?.();
    expect(await screen.findByRole("tab", { name: "OTel env vars" })).toBeInTheDocument();
  } finally {
    release?.();
  }
});

it("asks for one trace over all time before it shows onboarding", async () => {
  savedViewHandlers([]);
  const probes: URL[] = [];
  server.use(
    http.get("/api/v1/projects/:project/traces", ({ request }) => {
      const url = new URL(request.url);
      if (!url.searchParams.has("from")) {
        probes.push(url);
      }
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByRole("tab", { name: "OTel env vars" })).toBeInTheDocument();
  expect(probes[0]?.searchParams.get("limit")).toBe("1");
  expect(probes[0]?.searchParams.has("filter")).toBe(false);
  expect(probes[0]?.searchParams.has("include_eval")).toBe(false);
});

it("shows the empty range, not onboarding, when the project has traces outside it", async () => {
  savedViewHandlers([]);
  setOnboardingPollMs(20);
  const dataset = createDataset({ seed: 1, now: Date.now(), traces: 1 });
  const trace = dataset.traces[0];
  if (!trace) {
    throw new Error("missing trace");
  }
  const listed: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces") && url.searchParams.has("from")) {
      listed.push(url);
    }
  };
  server.events.on("request:start", onStart);
  server.use(
    http.get("/api/v1/projects/:project/traces", ({ request }) => {
      const url = new URL(request.url);
      const probe = !url.searchParams.has("from");
      return HttpResponse.json({ data: probe ? [trace] : [], next_cursor: null });
    }),
  );
  try {
    await renderApp("/acme/support-bot/traces");
    expect(await screen.findByText("No traces in this time range.")).toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "OTel env vars" })).not.toBeInTheDocument();
    const before = listed.length;
    await new Promise((resolve) => setTimeout(resolve, 150));
    expect(listed.length).toBe(before);
  } finally {
    server.events.removeListener("request:start", onStart);
    setOnboardingPollMs(ONBOARDING_POLL_MS);
  }
});

it("opens the trace stream with the list filter and search", async () => {
  const streams: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces/stream")) {
      streams.push(url);
    }
  };
  server.events.on("request:start", onStart);
  server.use(traceStreamHandler(() => undefined));
  try {
    await renderApp(
      `/acme/support-bot/traces?live=1&filter=${encodeURIComponent("status = 'error'")}&search=refund&eval=1&exp=1`,
    );
    await waitFor(() => expect(streams.length).toBeGreaterThan(0));
    expect(streams[0]?.searchParams.get("filter")).toBe("status = 'error'");
    expect(streams[0]?.searchParams.get("search")).toBe("refund");
    expect(streams[0]?.searchParams.get("include_eval")).toBe("true");
    expect(streams[0]?.searchParams.get("include_experiments")).toBe("true");
    expect(screen.getByRole("switch", { name: "Live" })).toBeEnabled();
    expect(screen.getByRole("switch", { name: "Live" })).toBeChecked();
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("opens no trace stream while the list answers an invalid filter", async () => {
  savedViewHandlers([]);
  const streams: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces/stream")) {
      streams.push(url);
    }
  };
  server.events.on("request:start", onStart);
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      problemResponse(400, "invalid_filter", { detail: "unknown column" }),
    ),
    traceStreamHandler(() => undefined),
  );
  try {
    await renderApp(`/acme/support-bot/traces?live=1&filter=${encodeURIComponent("nope =")}`);
    expect((await screen.findAllByText("unknown column")).length).toBeGreaterThan(0);
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(streams).toEqual([]);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("keeps the live stream open while a background refetch fails", async () => {
  const streams: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces/stream")) {
      streams.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { emit } = await openLive("/acme/support-bot/traces?live=1");
    const opened = streams.length;
    server.use(http.get("/api/v1/projects/:project/traces", () => HttpResponse.error()));
    emit.resync();
    await new Promise((resolve) => setTimeout(resolve, 80));
    emit.trace(liveTrace("c1000000000000000000000000000051", "after the blip", LATER));
    expect(await screen.findByText("after the blip")).toBeInTheDocument();
    expect(streams.length).toBe(opened);
    expect(screen.getByRole("switch", { name: "Live" })).toBeChecked();
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("prepends a new trace and highlights the row", async () => {
  const { emit } = await openLive("/acme/support-bot/traces?live=1");
  emit.trace(liveTrace("c1000000000000000000000000000001", "live arrival", LATER));
  const name = await screen.findByText("live arrival");
  expect(name.closest("[role=row]")).toHaveClass("live-row");
  expect(screen.getByText("6 loaded")).toBeInTheDocument();
});

it("inserts a trace that finished after a newer one at its start-time place", async () => {
  const { emit } = await openLive("/acme/support-bot/traces?live=1");
  emit.trace(liveTrace("c1000000000000000000000000000006", "newer arrival", LATER_STILL));
  await screen.findByText("newer arrival");
  emit.trace(liveTrace("c1000000000000000000000000000007", "late arrival", LATER));
  const late = await screen.findByText("late arrival");
  expect(late.closest("[role=row]")).toHaveClass("live-row");
  expect(screen.getByText("7 loaded")).toBeInTheDocument();
  const rows = screen.getAllByRole("row").map((row) => row.textContent ?? "");
  const newer = rows.findIndex((text) => text.includes("newer arrival"));
  expect(rows[newer + 1]).toContain("late arrival");
});

it("replaces a known trace in place", async () => {
  const { emit } = await openLive("/acme/support-bot/traces?live=1");
  emit.trace(liveTrace(FIRST, "renamed in place", "2026-01-01T10:30:00.000000000Z"));
  expect(await screen.findByText("renamed in place")).toBeInTheDocument();
  // The facet panel lists the same name as a value, so the check is scoped to the table.
  const table = screen.getByRole("grid", { name: "Traces" });
  expect(within(table).queryByText("chat claude-sonnet-4-5")).not.toBeInTheDocument();
  expect(screen.getByText("renamed in place").closest("[role=row]")).not.toHaveClass("live-row");
  expect(screen.getByText("5 loaded")).toBeInTheDocument();
});

it("buffers traces while the list is scrolled and inserts them from the pill", async () => {
  const { emit } = await openLive("/acme/support-bot/traces?live=1");
  const scroller = tracesScroller();
  scroller.scrollTop = 400;
  fireEvent.scroll(scroller);
  emit.trace(liveTrace("c1000000000000000000000000000002", "buffered one", LATER));
  emit.trace(liveTrace("c1000000000000000000000000000003", "buffered two", LATER_STILL));
  const pill = await screen.findByRole("button", { name: "2 new traces ↑" });
  expect(screen.queryByText("buffered one")).not.toBeInTheDocument();
  expect(screen.queryByText("buffered two")).not.toBeInTheDocument();
  fireEvent.click(pill);
  expect(await screen.findByText("buffered one")).toBeInTheDocument();
  expect(screen.getByText("buffered two")).toBeInTheDocument();
  expect(scroller.scrollTop).toBe(0);
});

it("inserts buffered traces when the list scrolls back to the top", async () => {
  const { emit } = await openLive("/acme/support-bot/traces?live=1");
  const scroller = tracesScroller();
  scroller.scrollTop = 400;
  emit.trace(liveTrace("c1000000000000000000000000000004", "scrolled arrival", LATER));
  expect(await screen.findByRole("button", { name: "1 new trace ↑" })).toBeInTheDocument();
  scroller.scrollTop = 0;
  fireEvent.scroll(scroller);
  expect(await screen.findByText("scrolled arrival")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "1 new trace ↑" })).not.toBeInTheDocument();
});

it("refetches the list when the stream resyncs", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { emit } = await openLive("/acme/support-bot/traces?live=1");
    const before = urls.length;
    expect(before).toBeGreaterThan(0);
    emit.resync();
    await waitFor(() => expect(urls.length).toBeGreaterThan(before));
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("keeps the selected row in place when a trace is prepended", async () => {
  server.use(spanStreamHandler(() => undefined));
  const { emit } = await openLive(`/acme/support-bot/traces?live=1&trace=${FIRST}`);
  const scroller = tracesScroller();
  expect(scroller.scrollTop).toBe(0);
  emit.trace(liveTrace("c1000000000000000000000000000005", "above selection", LATER));
  await waitFor(() => expect(screen.getByText("6 loaded")).toBeInTheDocument());
  expect(scroller.scrollTop).toBe(ROW_HEIGHT);
});

it("stops applying traces after live is turned off", async () => {
  const { emit, user } = await openLive("/acme/support-bot/traces?live=1");
  const toggle = screen.getByRole("switch", { name: "Live" });
  await user.click(toggle);
  await waitFor(() => expect(toggle).not.toBeChecked());
  emit.trace(liveTrace("c1000000000000000000000000000099", "ghost trace", LATER));
  await new Promise((resolve) => setTimeout(resolve, 80));
  expect(screen.queryByText("ghost trace")).not.toBeInTheDocument();
});

it("disables live for an absolute range", async () => {
  const streams: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces/stream")) {
      streams.push(url);
    }
  };
  server.events.on("request:start", onStart);
  server.use(traceStreamHandler(() => undefined));
  try {
    const { user } = await renderApp(
      "/acme/support-bot/traces?live=1&from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z",
    );
    const toggle = await screen.findByRole("switch", { name: "Live" });
    expect(toggle).toBeDisabled();
    await user.hover(screen.getByText("Live"));
    expect(await screen.findByRole("tooltip")).toHaveTextContent(
      "Live needs a relative time range",
    );
    expect(streams).toEqual([]);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("disables live when the list is not sorted by start time descending", async () => {
  const { user } = await renderApp("/acme/support-bot/traces?sort=cost_total");
  const toggle = await screen.findByRole("switch", { name: "Live" });
  expect(toggle).toBeDisabled();
  await user.hover(screen.getByText("Live"));
  expect(await screen.findByRole("tooltip")).toHaveTextContent(
    "Live follows start time, newest first",
  );
});

it("re-anchors the range when live is turned on", async () => {
  const urls: URL[] = [];
  const streams: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces/stream")) {
      streams.push(url);
    } else if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  server.use(traceStreamHandler(() => undefined));
  try {
    const { user } = await renderApp("/acme/support-bot/traces?range=24h");
    const toggle = await screen.findByRole("switch", { name: "Live" });
    expect(toggle).toBeEnabled();
    const before = urls.length;
    const to = urls.at(-1)?.searchParams.get("to");
    const clock = vi.spyOn(Date, "now").mockReturnValue(Date.now() + 60_000);
    try {
      await user.click(toggle);
      await waitFor(() => expect(toggle).toBeChecked());
      await waitFor(() => expect(urls.length).toBeGreaterThan(before));
      expect(urls.at(-1)?.searchParams.get("to")).not.toBe(to);
      await waitFor(() => expect(streams.length).toBeGreaterThan(0));
    } finally {
      clock.mockRestore();
    }
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("caps the paused buffer and refetches the first page on reveal", async () => {
  setLiveLimits({ buffer: 2, page: LIVE_PAGE_MAX });
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) urls.push(url);
  };
  server.events.on("request:start", onStart);
  try {
    const { emit } = await openLive("/acme/support-bot/traces?live=1");
    const scroller = tracesScroller();
    scroller.scrollTop = 400;
    fireEvent.scroll(scroller);
    emit.trace(liveTrace("c1000000000000000000000000000011", "first over", LATER));
    emit.trace(liveTrace("c1000000000000000000000000000012", "second over", LATER));
    emit.trace(liveTrace("c1000000000000000000000000000013", "third over", LATER_STILL));
    const pill = await screen.findByRole("button", { name: "2+ new traces ↑" });
    const before = urls.length;
    fireEvent.click(pill);
    await waitFor(() => expect(urls.length).toBeGreaterThan(before));
    expect(scroller.scrollTop).toBe(0);
    expect(screen.queryByRole("button", { name: /new traces/ })).not.toBeInTheDocument();
    expect(await screen.findByText("5 loaded")).toBeInTheDocument();
  } finally {
    server.events.removeListener("request:start", onStart);
    setLiveLimits({ buffer: LIVE_BUFFER_MAX, page: LIVE_PAGE_MAX });
  }
});

it("refetches the first page instead of growing it past the cap", async () => {
  setLiveLimits({ buffer: LIVE_BUFFER_MAX, page: 5 });
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) urls.push(url);
  };
  server.events.on("request:start", onStart);
  try {
    const { emit } = await openLive("/acme/support-bot/traces?live=1");
    expect(screen.getByText("5 loaded")).toBeInTheDocument();
    const before = urls.length;
    emit.trace(liveTrace("c1000000000000000000000000000014", "over the cap", LATER));
    await waitFor(() => expect(urls.length).toBeGreaterThan(before));
    await waitFor(() => expect(screen.getByText("5 loaded")).toBeInTheDocument());
    expect(screen.queryByText("6 loaded")).not.toBeInTheDocument();
  } finally {
    server.events.removeListener("request:start", onStart);
    setLiveLimits({ buffer: LIVE_BUFFER_MAX, page: LIVE_PAGE_MAX });
  }
});

it("keeps one first-page refetch in flight and queues one trailing refetch", async () => {
  setLiveLimits({ buffer: LIVE_BUFFER_MAX, page: 5 });
  const { emit } = await openLive("/acme/support-bot/traces?live=1");
  expect(screen.getByText("5 loaded")).toBeInTheDocument();
  let requests = 0;
  let inFlight = 0;
  let maxInFlight = 0;
  let aborted = 0;
  let release: (() => void) | undefined;
  server.use(
    http.get("/api/v1/projects/:project/traces", async ({ request }) => {
      requests += 1;
      const call = requests;
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      request.signal.addEventListener("abort", () => {
        aborted += 1;
      });
      try {
        if (call === 1) {
          await new Promise<void>((resolve) => {
            release = resolve;
          });
        }
        const name = call === 1 ? "first refresh" : "after the burst";
        return HttpResponse.json({
          data: [liveTrace(`c10000000000000000000000000000${call}0`, name, LATER)],
          next_cursor: null,
        });
      } finally {
        inFlight -= 1;
      }
    }),
  );
  try {
    emit.trace(liveTrace("c1000000000000000000000000000021", "burst one", LATER));
    await waitFor(() => expect(requests).toBe(1));
    emit.trace(liveTrace("c1000000000000000000000000000022", "burst two", LATER));
    emit.trace(liveTrace("c1000000000000000000000000000023", "burst three", LATER));
    emit.resync();
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(requests).toBe(1);
    expect(release).toBeDefined();
    release?.();
    expect(await screen.findByText("after the burst")).toBeInTheDocument();
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(requests).toBe(2);
    expect(maxInFlight).toBe(1);
    expect(aborted).toBe(0);
    expect(screen.queryByText("first refresh")).not.toBeInTheDocument();
  } finally {
    release?.();
    setLiveLimits({ buffer: LIVE_BUFFER_MAX, page: LIVE_PAGE_MAX });
  }
});

it("queues a trace that arrives during a first-page refetch instead of inserting it", async () => {
  setLiveLimits({ buffer: LIVE_BUFFER_MAX, page: LIVE_PAGE_MAX });
  const { emit } = await openLive("/acme/support-bot/traces?live=1");
  expect(screen.getByText("5 loaded")).toBeInTheDocument();
  let requests = 0;
  let inFlight = 0;
  let maxInFlight = 0;
  let aborted = 0;
  let release: (() => void) | undefined;
  server.use(
    http.get("/api/v1/projects/:project/traces", async ({ request }) => {
      requests += 1;
      const call = requests;
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      request.signal.addEventListener("abort", () => {
        aborted += 1;
      });
      try {
        if (call === 1) {
          await new Promise<void>((resolve) => {
            release = resolve;
          });
        }
        const rows =
          call === 1
            ? [liveTrace("c1000000000000000000000000000041", "first refresh", LATER)]
            : [liveTrace("c1000000000000000000000000000042", "arrived mid-fetch", LATER)];
        return HttpResponse.json({ data: rows, next_cursor: null });
      } finally {
        inFlight -= 1;
      }
    }),
  );
  try {
    emit.resync();
    await waitFor(() => expect(requests).toBe(1));
    emit.trace(liveTrace("c1000000000000000000000000000043", "arrived mid-fetch", LATER));
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(screen.queryByText("arrived mid-fetch")).not.toBeInTheDocument();
    expect(requests).toBe(1);
    release?.();
    expect(await screen.findByText("arrived mid-fetch")).toBeInTheDocument();
    expect(requests).toBe(2);
    expect(maxInFlight).toBe(1);
    expect(aborted).toBe(0);
    expect(screen.queryByText("first refresh")).not.toBeInTheDocument();
  } finally {
    release?.();
    setLiveLimits({ buffer: LIVE_BUFFER_MAX, page: LIVE_PAGE_MAX });
  }
});

it("keeps the pages a reader scrolled to when a queued refetch runs", async () => {
  server.use(...largeHandlers(traces5k));
  let emit: StreamEmit | undefined;
  server.use(
    traceStreamHandler((next) => {
      emit = next;
      return undefined;
    }),
  );
  let hold = false;
  let release: (() => void) | undefined;
  const cursors: string[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const cursor = new URL(request.url).searchParams.get("cursor");
    if (cursor) {
      cursors.push(cursor);
    }
  };
  server.events.on("request:start", onStart);
  // Holds the next first-page request; the handler below falls through to the large list.
  server.use(
    http.get("/api/v1/projects/:project/traces", async ({ request }) => {
      if (hold && new URL(request.url).searchParams.has("from")) {
        hold = false;
        await new Promise<void>((resolve) => {
          release = resolve;
        });
      }
      return undefined;
    }),
  );
  try {
    const { queryClient } = await renderApp("/acme/support-bot/traces?live=1");
    await screen.findByRole("grid", { name: "Traces" });
    await waitFor(() => expect(emit).toBeDefined());
    const scroller = tracesScroller();
    const firstPage = loadedCount();
    scroller.scrollTop = scroller.scrollHeight;
    fireEvent.scroll(scroller);
    await waitFor(() => expect(loadedCount()).toBeGreaterThan(firstPage));
    const scrolled = loadedCount();
    // A list refetch is in flight at the top, and a trace queues one trailing refetch.
    scroller.scrollTop = 0;
    fireEvent.scroll(scroller);
    const list = queryClient
      .getQueryCache()
      .getAll()
      .find((query) => (query.state.data as { pages?: unknown } | undefined)?.pages !== undefined);
    if (!list) {
      throw new Error("missing list query");
    }
    hold = true;
    void queryClient.invalidateQueries({ queryKey: list.queryKey, exact: true });
    await waitFor(() => expect(release).toBeDefined());
    emit?.trace(liveTrace("c1000000000000000000000000000061", "queued top", LATER));
    await new Promise((resolve) => setTimeout(resolve, 40));
    // The reader scrolls down before the refetch settles.
    scroller.scrollTop = ROW_HEIGHT * 10;
    fireEvent.scroll(scroller);
    const cursorsBefore = cursors.length;
    release?.();
    await waitFor(() => expect(cursors.length).toBeGreaterThan(cursorsBefore));
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(loadedCount()).toBeGreaterThanOrEqual(scrolled);
  } finally {
    release?.();
    server.events.removeListener("request:start", onStart);
  }
  // It renders 5k traces and takes about 11 s alone: twice the default for a loaded machine.
}, 30_000);

function loadedCount(): number {
  const text = screen.getByText(/^\d+ loaded$/).textContent ?? "";
  return Number.parseInt(text, 10);
}

const LATER = "2026-11-11T21:00:00.000000000Z";
const LATER_STILL = "2026-11-11T21:00:01.000000000Z";

function liveTrace(id: string, name: string, start: string): Schema<"TraceSummary"> {
  return {
    trace_id: id,
    name,
    start_time: start,
    end_time: start,
    duration_ns: 1_000_000_000,
    status: "ok",
    has_root: true,
    error_count: 0,
    llm_count: 0,
    span_count: 1,
    metadata: {},
    models: [],
    tags: [],
    scores: [],
  };
}

async function openLive(url: string) {
  let emit: StreamEmit | undefined;
  server.use(
    traceStreamHandler((next) => {
      emit = next;
      return undefined;
    }),
  );
  const view = await renderApp(url);
  await screen.findByRole("grid", { name: "Traces" });
  await waitFor(() => expect(emit).toBeDefined());
  if (!emit) {
    throw new Error("stream did not open");
  }
  return { ...view, emit };
}

function tracesScroller(): HTMLElement {
  const node = document.querySelector<HTMLElement>('[aria-label="Traces"] [data-scroll-container]');
  if (!node) {
    throw new Error("missing traces scroller");
  }
  return node;
}

function stubClipboard() {
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: { writeText },
  });
  return writeText;
}

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

it("previews the delete of the traces the list shows", async () => {
  let previewBody: unknown;
  server.use(
    http.post("/api/v1/projects/:project/deletions/preview", async ({ request }) => {
      previewBody = await request.json();
      return HttpResponse.json({ matched_traces: 2, capped: false });
    }),
  );
  const { user } = await renderApp(
    "/acme/support-bot/traces?filter=status%20%3D%20'error'&search=refund&range=24h&eval=1",
  );
  await user.click(await screen.findByRole("button", { name: "Delete matching…" }));

  expect(await screen.findByText(/^2 traces match/)).toBeInTheDocument();
  const body = previewBody as { from: string; to: string };
  expect(body).toMatchObject({
    filter: "status = 'error'",
    search: "refund",
    include_eval: true,
    include_experiments: false,
  });
  expect(Date.parse(body.from)).toBeLessThan(Date.parse(body.to));
});

it("disables delete matching for members, with the reason", async () => {
  server.use(projectAs("member"));
  await renderApp("/acme/support-bot/traces");

  const button = await screen.findByRole("button", { name: "Delete matching…" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only project admins can delete traces");
});

it("shows the layout switcher in the open trace's header, not in the list toolbar", async () => {
  await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  expect(screen.queryByRole("radiogroup", { name: "Layout" })).not.toBeInTheDocument();
  cleanup();
  await renderApp("/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000001");
  await screen.findByRole("grid", { name: "Trace" });
  expect(screen.getByRole("radiogroup", { name: "Layout" })).toBeInTheDocument();
});

it("shows the list and the trace together in bottom mode", async () => {
  localStorage.setItem("cetra.list_layout", "bottom");
  await renderApp("/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000001");
  expect(await screen.findByRole("grid", { name: "Trace" })).toBeInTheDocument();
  expect(screen.getByRole("grid", { name: "Traces" })).toBeInTheDocument();
});

it("shows the trace in a drawer over the list, and Escape closes it", async () => {
  localStorage.setItem("cetra.list_layout", "drawer");
  const { router } = await renderApp(
    "/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000001",
  );
  const trace = await screen.findByRole("grid", { name: "Trace" });
  expect(screen.getByRole("grid", { name: "Traces" })).toBeInTheDocument();
  expect(screen.getByRole("complementary", { name: "Trace" })).toContainElement(trace);

  fireEvent.keyDown(window, { key: "Escape" });
  await waitFor(() => {
    expect(router.state.location.search.trace).toBeUndefined();
  });
  await waitFor(() => {
    expect(screen.queryByRole("complementary", { name: "Trace" })).not.toBeInTheDocument();
  });
  expect(screen.getByRole("grid", { name: "Traces" })).toBeInTheDocument();
});

it("prefetches the trace detail and its root span when the pointer rests on a row", async () => {
  const detailRequests: string[] = [];
  const rootRequests: string[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith(`/traces/${TRACE_1}`)) {
      detailRequests.push(url.pathname);
    }
    if (url.pathname.includes(`/traces/${TRACE_1}/spans/`)) {
      rootRequests.push(url.pathname);
    }
  };
  server.events.on("request:start", onStart);
  try {
    await renderApp("/acme/support-bot/traces");
    const list = await screen.findByRole("grid", { name: "Traces" });
    const scroller = list.querySelector("[data-scroll-container]");
    if (!(scroller instanceof HTMLElement)) {
      throw new Error("missing scroll container");
    }
    scroller.scrollTop = 240;
    fireEvent.scroll(scroller);
    const row = within(list).getByRole("row", { name: /4\.20s/ });
    fireEvent.pointerEnter(row);
    await waitFor(() => expect(detailRequests).toHaveLength(1));
    await waitFor(() => expect(rootRequests).toHaveLength(1));
    await new Promise((resolve) => setTimeout(resolve, 150));
    expect(detailRequests).toHaveLength(1);
    expect(rootRequests).toHaveLength(1);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("prefetches the neighbours of an open trace once", async () => {
  const requested: string[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.includes("/traces/a1000000000000000000000000000")) {
      requested.push(url.pathname.split("/").at(-1) ?? "");
    }
  };
  server.events.on("request:start", onStart);
  try {
    await renderApp("/acme/support-bot/traces?range=1h&trace=a1000000000000000000000000000003");
    await screen.findByRole("grid", { name: "Trace" });
    await waitFor(() => {
      expect(requested).toEqual(expect.arrayContaining([NEXT_OF_3, PREVIOUS_OF_3]));
    });
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(requested.filter((id) => id === NEXT_OF_3)).toHaveLength(1);
    expect(requested.filter((id) => id === PREVIOUS_OF_3)).toHaveLength(1);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("returns from the full trace page to the same list search", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces?sort=duration&order=asc");
  const grid = await screen.findByRole("grid", { name: "Traces" });
  const firstRow = grid.querySelector<HTMLElement>('[role="row"][aria-rowindex="2"]');
  if (!firstRow) {
    throw new Error("no first trace row");
  }
  fireEvent.click(firstRow);
  await user.click(await screen.findByRole("link", { name: "Open full page" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toMatch(/\/traces\/[0-9a-f]{32}$/);
  });
  await user.click(await screen.findByRole("link", { name: "Back to traces" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/traces");
  });
  expect(router.state.location.search).toMatchObject({ sort: "duration", order: "asc" });
});

it("does not apply the default view on a return from a detail page", async () => {
  const { router } = await renderApp("/acme/support-bot/sessions");
  await screen.findByRole("grid", { name: "Sessions" });
  await router.navigate({
    to: "/$org/$project/traces",
    params: { org: "acme", project: "support-bot" },
    state: { listRestore: {} },
  });
  const traces = await screen.findByRole("grid", { name: "Traces" });
  expect(within(traces).getAllByRole("row").length).toBeGreaterThan(1);
  expect(router.state.location.search).toEqual({});
});

/** The row checkbox of the `index`th loaded row. */
async function rowBox(index: number): Promise<HTMLElement> {
  const grid = await screen.findByRole("grid", { name: "Traces" });
  const boxes = within(grid).getAllByRole("checkbox", { name: "Select row" });
  const box = boxes[index];
  if (!box) {
    throw new Error(`no row checkbox ${index}`);
  }
  return box;
}

it("checks trace rows without opening them and offers to add them to a dataset", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  expect(screen.queryByRole("button", { name: /to dataset$/ })).not.toBeInTheDocument();

  await user.click(await rowBox(0));
  await user.click(await rowBox(1));
  expect(screen.getByRole("button", { name: "Add 2 to dataset" })).toBeEnabled();

  await user.click(await rowBox(0));
  expect(screen.getByRole("button", { name: "Add 1 to dataset" })).toBeEnabled();
  expect(router.state.location.search.trace).toBeUndefined();
});

it("keeps the bulk actions out of the breadcrumb", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await user.click(await rowBox(0));

  const crumb = await screen.findByRole("navigation", { name: "Breadcrumb" });
  expect(within(crumb).queryByRole("button", { name: "Add 1 to dataset" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Add 1 to dataset" })).toBeInTheDocument();
});

it("disables the add action for viewers and says why", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  const { user } = await renderApp("/acme/support-bot/traces");
  await user.click(await rowBox(0));

  expect(screen.getByRole("button", { name: "Add 1 to dataset" })).toBeDisabled();
  expect(screen.getByText("Only members can add to datasets")).toBeInTheDocument();
});

it("clears the checked traces when the search changes", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await user.click(await rowBox(0));
  expect(screen.getByRole("button", { name: "Add 1 to dataset" })).toBeInTheDocument();

  await user.type(screen.getByRole("searchbox", { name: "Search" }), "refund");

  await waitFor(() =>
    expect(screen.queryByRole("button", { name: "Add 1 to dataset" })).not.toBeInTheDocument(),
  );
});

it("keeps the facet panel and its typed value search when the filter changes", async () => {
  server.use(
    facetsHandler((ask) => {
      const values =
        ask.field === "status"
          ? [
              { value: "ok", count: 100 },
              { value: "error", count: 17 },
            ]
          : [];
      return {
        kind: "values",
        total: values.reduce((sum, item) => sum + item.count, 0),
        missing: 0,
        values,
        more: false,
      };
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/traces?range=15m");
  const panel = await screen.findByRole("complementary", { name: "Filter facets" });
  await user.type(within(panel).getByRole("searchbox", { name: "Search Status values" }), "er");
  await user.click(await within(panel).findByRole("checkbox", { name: "error" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'error'");
  });
  // The same panel node, with the typed text still in its value search.
  expect(screen.getByRole("complementary", { name: "Filter facets" })).toBe(panel);
  expect(within(panel).getByRole("searchbox", { name: "Search Status values" })).toHaveValue("er");
});

const QUEUE_LIST = "/api/v1/projects/:project/annotation-queues";
const QUEUE_ITEMS = "/api/v1/projects/:project/annotation-queues/:id/items";
const QUEUE_PREVIEW = "/api/v1/projects/:project/annotation-queues/:id/items/matching/preview";
const QUEUE: Schema<"AnnotationQueue"> = queuesFixture.data[0] as Schema<"AnnotationQueue">;

/** Serves one trace queue for every list request. */
function serveQueue() {
  server.use(http.get(QUEUE_LIST, () => HttpResponse.json({ data: [QUEUE], next_cursor: null })));
}

/** Records the body of each add to a queue. */
function captureQueueAdds() {
  const calls: { id: string; body: { items: { trace_id?: string }[] } }[] = [];
  server.use(
    http.post(QUEUE_ITEMS, async ({ request, params }) => {
      const body = (await request.json()) as { items: { trace_id?: string }[] };
      calls.push({ id: String(params.id), body });
      return HttpResponse.json(
        { added: body.items.length, existing: 0, skipped: [] },
        { status: 201 },
      );
    }),
  );
  return calls;
}

it("adds the checked traces to a trace queue", async () => {
  serveQueue();
  const calls = captureQueueAdds();
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await user.click(await rowBox(0));
  await user.click(await rowBox(1));
  await user.click(screen.getByRole("button", { name: "Add 2 to queue" }));

  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
  await user.click(await screen.findByRole("option", { name: QUEUE.name }));
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(await within(dialog).findByText(`Added 2 to ${QUEUE.name}.`)).toBeInTheDocument();
  expect(calls).toHaveLength(1);
  expect(calls[0]?.id).toBe(QUEUE.id);
  expect(calls[0]?.body.items).toHaveLength(2);
  for (const item of calls[0]?.body.items ?? []) {
    expect(item.trace_id).toMatch(/^[0-9a-f]{32}$/);
  }
  expect(router.state.location.search.trace).toBeUndefined();
});

it("adds the traces the list matches to a queue with the list's filter and search", async () => {
  serveQueue();
  const previews: unknown[] = [];
  server.use(
    http.post(QUEUE_PREVIEW, async ({ request }) => {
      previews.push(await request.json());
      return HttpResponse.json({ matched: 2, capped: false });
    }),
  );
  const { user } = await renderApp(
    "/acme/support-bot/traces?filter=status%20%3D%20'error'&search=refund&range=24h&exp=1",
  );
  await user.click(await screen.findByRole("button", { name: "Add matching to queue…" }));

  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
  await user.click(await screen.findByRole("option", { name: QUEUE.name }));

  expect(await within(dialog).findByText("2 traces match.")).toBeInTheDocument();
  expect(previews).toHaveLength(1);
  expect(previews[0]).toMatchObject({
    filter: "status = 'error'",
    search: "refund",
    include_experiments: true,
  });
});

it("disables the queue actions for viewers and says why", async () => {
  server.use(projectAs("viewer"));
  await renderApp("/acme/support-bot/traces");

  const button = await screen.findByRole("button", { name: "Add matching to queue…" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only members can add to queues");
});

const TRACES_ENDPOINT = "*/api/v1/projects/:project/traces";
const TRACES_PATH = { org: "acme", project: "support-bot" } as const;
const FIRST_NAME = "chat claude-sonnet-4-5";

type TracesRouter = Awaited<ReturnType<typeof renderApp>>["router"];

/** A list request: the first page of the window (limit 100). */
function isListRequest(url: URL): boolean {
  return url.pathname.endsWith("/traces") && url.searchParams.get("limit") === "100";
}

/** A new-rows check request (limit 50). */
function isCheckRequest(url: URL): boolean {
  return url.pathname.endsWith("/traces") && url.searchParams.get("limit") === "50";
}

async function leaveTraces(router: TracesRouter) {
  await router.navigate({ to: "/$org/$project/sessions", params: TRACES_PATH });
}

async function returnToTraces(router: TracesRouter, search: { live?: 1 } = {}) {
  await router.navigate({ to: "/$org/$project/traces", params: TRACES_PATH, search });
  await screen.findByRole("grid", { name: "Traces" });
}

it("a return within 15 minutes sends no list request", async () => {
  const { router } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await leaveTraces(router);
  const recording = recordUrls();
  try {
    await returnToTraces(router);
    expect(screen.getByRole("grid", { name: "Traces" })).toHaveTextContent(FIRST_NAME);
    expect(recording.urls.some(isListRequest)).toBe(false);
  } finally {
    recording.stop();
  }
});

it("the pill shows the new traces and loads them", async () => {
  server.use(
    http.get(TRACES_ENDPOINT, ({ request }) => {
      if (new URL(request.url).searchParams.get("limit") === "50") {
        return HttpResponse.json({
          ...tracesFixture,
          data: tracesFixture.data.slice(0, 2),
          next_cursor: null,
        });
      }
      return undefined;
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await leaveTraces(router);
  await returnToTraces(router);
  const pill = await screen.findByRole("button", { name: "2 new traces ↑" });

  const recording = recordUrls();
  try {
    await user.click(pill);
    await waitFor(() => {
      expect(recording.urls.some(isListRequest)).toBe(true);
    });
    await waitFor(() => {
      expect(screen.getByRole("grid", { name: "Traces" })).not.toHaveAttribute("aria-busy");
    });
    expect(screen.queryByRole("button", { name: /new trace/ })).not.toBeInTheDocument();
  } finally {
    recording.stop();
  }
});

it("a single new trace reads in the singular", async () => {
  server.use(
    http.get(TRACES_ENDPOINT, ({ request }) => {
      if (new URL(request.url).searchParams.get("limit") === "50") {
        return HttpResponse.json({
          ...tracesFixture,
          data: tracesFixture.data.slice(0, 1),
          next_cursor: null,
        });
      }
      return undefined;
    }),
  );
  const { router } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await leaveTraces(router);
  await returnToTraces(router);
  expect(await screen.findByRole("button", { name: "1 new trace ↑" })).toBeInTheDocument();
});

it("no pill without new traces", async () => {
  server.use(
    http.get(TRACES_ENDPOINT, ({ request }) => {
      if (new URL(request.url).searchParams.get("limit") === "50") {
        return HttpResponse.json({ data: [], next_cursor: null });
      }
      return undefined;
    }),
  );
  const { router, queryClient } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await leaveTraces(router);
  const recording = recordUrls();
  try {
    await returnToTraces(router);
    await waitFor(() => {
      expect(recording.urls.some(isCheckRequest)).toBe(true);
    });
    // The empty answer has landed once nothing is fetching.
    await waitFor(() => expect(queryClient.isFetching()).toBe(0));
    expect(screen.queryByRole("button", { name: /new trace/ })).not.toBeInTheDocument();
  } finally {
    recording.stop();
  }
});

it("an expired anchor refetches", async () => {
  vi.useFakeTimers({ toFake: ["Date"], shouldAdvanceTime: true });
  try {
    const { router } = await renderApp("/acme/support-bot/traces");
    await screen.findByRole("grid", { name: "Traces" });
    await leaveTraces(router);
    vi.setSystemTime(Date.now() + 16 * 60_000);
    const recording = recordUrls();
    try {
      await returnToTraces(router);
      await waitFor(() => {
        expect(recording.urls.some(isListRequest)).toBe(true);
      });
      expect(recording.urls.some(isCheckRequest)).toBe(false);
    } finally {
      recording.stop();
    }
  } finally {
    vi.useRealTimers();
  }
});

it("a new window keeps the old rows dimmed", async () => {
  server.use(
    http.get(TRACES_ENDPOINT, async ({ request }) => {
      if (new URL(request.url).searchParams.has("filter")) {
        await new Promise(() => {});
      }
      return undefined;
    }),
  );
  const { router } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await router.navigate({
    to: "/$org/$project/traces",
    params: TRACES_PATH,
    search: { filter: "status = 'error'" },
  });
  const grid = screen.getByRole("grid", { name: "Traces" });
  await waitFor(() => {
    expect(grid).toHaveAttribute("aria-busy", "true");
  });
  expect(grid).toHaveTextContent(FIRST_NAME);
});

it("an absolute range sends no list and no check request", async () => {
  const from = "2026-01-01T09:00:00.000Z";
  const to = "2026-01-01T11:00:00.000Z";
  const { router } = await renderApp(`/acme/support-bot/traces?from=${from}&to=${to}`);
  await screen.findByRole("grid", { name: "Traces" });
  await leaveTraces(router);
  const recording = recordUrls();
  try {
    await router.navigate({
      to: "/$org/$project/traces",
      params: TRACES_PATH,
      search: { from, to },
    });
    await screen.findByRole("grid", { name: "Traces" });
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(recording.urls.some(isListRequest)).toBe(false);
    expect(recording.urls.some(isCheckRequest)).toBe(false);
  } finally {
    recording.stop();
  }
});

it("refresh sets a new window", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  const recording = recordUrls();
  try {
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => {
      expect(recording.urls.filter(isListRequest)).toHaveLength(1);
    });
  } finally {
    recording.stop();
  }
});

it("live mode starts a new window on return", async () => {
  server.use(traceStreamHandler(() => undefined));
  const { router } = await renderApp("/acme/support-bot/traces?live=1");
  await screen.findByRole("grid", { name: "Traces" });
  await leaveTraces(router);
  const recording = recordUrls();
  try {
    await returnToTraces(router, { live: 1 });
    await waitFor(() => {
      expect(recording.urls.some(isListRequest)).toBe(true);
    });
    expect(recording.urls.some(isCheckRequest)).toBe(false);
  } finally {
    recording.stop();
  }
});

it("turning live off starts a new window", async () => {
  server.use(traceStreamHandler(() => undefined));
  const { router } = await renderApp("/acme/support-bot/traces?live=1");
  await screen.findByRole("grid", { name: "Traces" });
  const recording = recordUrls();
  try {
    await router.navigate({ to: "/$org/$project/traces", params: TRACES_PATH, search: {} });
    await waitFor(() => {
      expect(recording.urls.some(isListRequest)).toBe(true);
    });
  } finally {
    recording.stop();
  }
});

it("a return restores the first visible row", async () => {
  const { router } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  tracesScroller().scrollTop = 2 * ROW_HEIGHT;
  fireEvent.scroll(tracesScroller());
  // The scroll position is remembered once it has settled.
  await new Promise((resolve) => setTimeout(resolve, 200));
  await leaveTraces(router);
  await returnToTraces(router);
  await waitFor(() => {
    expect(tracesScroller().scrollTop).toBe(2 * ROW_HEIGHT);
  });
});

it("shows the list totals under the table", async () => {
  server.use(
    http.get("/api/v1/projects/:project/traces/totals", () =>
      HttpResponse.json({ trace_count: 203, error_trace_count: 3, cost_total: 4.12 }),
    ),
  );
  const recording = recordUrls();
  try {
    await renderApp("/acme/support-bot/traces?filter=status%20%3D%20'error'&search=order");
    await waitFor(() => {
      expect(screen.getByRole("status", { name: "Totals" }).textContent).toBe(
        "203 traces · 3 errors · $4.12",
      );
    });
    const totalsUrl = recording.urls.find((url) => url.pathname.endsWith("/traces/totals"));
    const listUrl = recording.urls.find(isListRequest);
    expect(totalsUrl?.searchParams.get("filter")).toBe("status = 'error'");
    expect(totalsUrl?.searchParams.get("search")).toBe("order");
    expect(totalsUrl?.searchParams.get("from")).toBe(listUrl?.searchParams.get("from"));
    expect(totalsUrl?.searchParams.get("to")).toBe(listUrl?.searchParams.get("to"));
  } finally {
    recording.stop();
  }
});

it("totals the checked rows", async () => {
  server.use(
    http.get("/api/v1/projects/:project/traces/totals", () =>
      HttpResponse.json({ trace_count: 203, error_trace_count: 3, cost_total: 4.12 }),
    ),
  );
  const { user } = await renderApp("/acme/support-bot/traces");
  await waitFor(() => {
    expect(screen.getByRole("status", { name: "Totals" }).textContent).toBe(
      "203 traces · 3 errors · $4.12",
    );
  });
  await user.click(await rowBox(0));
  await user.click(await rowBox(2));
  await waitFor(() => {
    expect(screen.getByRole("status", { name: "Totals" }).textContent).toBe(
      "2 of 203 selected · 1 error · $0.0027",
    );
  });
  await user.click(await rowBox(0));
  await user.click(await rowBox(2));
  await waitFor(() => {
    expect(screen.getByRole("status", { name: "Totals" }).textContent).toBe(
      "203 traces · 3 errors · $4.12",
    );
  });
});

it("says when the totals are unavailable", async () => {
  server.use(
    http.get("/api/v1/projects/:project/traces/totals", () =>
      problemResponse(500, "internal", { detail: "totals failed" }),
    ),
  );
  await renderApp("/acme/support-bot/traces");
  await waitFor(() => {
    expect(screen.getByRole("status", { name: "Totals" }).textContent).toBe("Totals unavailable");
  });
  expect(screen.getByRole("grid", { name: "Traces" })).toBeInTheDocument();
});
