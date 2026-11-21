import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import { recordUrls } from "@/test/requests";
import spansFixture from "../../../../../../fixtures/api/responses/spans.json";
import queuesFixture from "../../../../../../fixtures/api/static/annotation-queues.json";

const TRACE = "a1000000000000000000000000000006";
const SPAN = "b100000000000061";
const SPANS_ENDPOINT = "*/api/v1/projects/:project/spans";
const SPANS_PATH = { org: "acme", project: "support-bot" } as const;

type Router = Awaited<ReturnType<typeof renderApp>>["router"];

/** A list request: the first page of the window (limit 100). */
function isListRequest(url: URL): boolean {
  return url.pathname.endsWith("/spans") && url.searchParams.get("limit") === "100";
}

/** A new-rows check request (limit 50). */
function isCheckRequest(url: URL): boolean {
  return url.pathname.endsWith("/spans") && url.searchParams.get("limit") === "50";
}

async function leaveSpans(router: Router) {
  await router.navigate({ to: "/$org/$project/sessions", params: SPANS_PATH });
}

async function returnToSpans(router: Router) {
  await router.navigate({ to: "/$org/$project/spans", params: SPANS_PATH });
  await screen.findByRole("grid", { name: "Spans" });
}

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
  // The selection column has no column id: the slots count the columns only.
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
  const { router } = await renderApp("/acme/support-bot/spans");
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Spans"]);
  const grid = await screen.findByRole("grid", { name: "Spans" });
  stubHeaderSlots(grid);
  vi.useFakeTimers();
  try {
    // Kind is the second unlocked column; drop it on the Start slot.
    fireEvent.pointerDown(screen.getByRole("columnheader", { name: "Kind" }), {
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
    ).toEqual(["status", "kind", "start", "name"]);
  });
});

it("renders the fixture spans in the default columns", async () => {
  await renderApp("/acme/support-bot/spans");
  const grid = await screen.findByRole("grid", { name: "Spans" });
  expect(grid).toHaveTextContent("get_order_status");
  expect(grid).toHaveTextContent("vector-search");
  expect(screen.getByText("12 loaded")).toBeInTheDocument();
  expect(screen.getAllByText("600ms").length).toBeGreaterThan(0);
  expect(screen.getByText("Retriever")).toBeInTheDocument();
  for (const name of [
    "Start",
    "Kind",
    "Name",
    "Model",
    "Input",
    "Output",
    "Duration",
    "TTFT",
    "Tokens",
    "Cost",
    "Session",
    "User",
    "Env",
  ]) {
    expect(screen.getByRole("columnheader", { name })).toBeInTheDocument();
  }
  expect(screen.getByRole("columnheader", { name: "Status" })).toBeInTheDocument();
  expect(screen.getByRole("columnheader", { name: "Start" })).toHaveAttribute(
    "aria-sort",
    "descending",
  );
  expect(screen.queryByRole("columnheader", { name: "Provider" })).not.toBeInTheDocument();
  expect(screen.queryByText("v1.4.0")).not.toBeInTheDocument();
});

it("sends the filter to list_spans", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/spans")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    await renderApp("/acme/support-bot/spans?filter=status%20%3D%20%27error%27");
    await screen.findByRole("grid", { name: "Spans" });
    await waitFor(() => {
      expect(urls.some((url) => url.searchParams.get("filter") === "status = 'error'")).toBe(true);
    });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("opens the span's trace with that span selected", async () => {
  const urls: string[] = [];
  const onStart = ({ request }: { request: Request }) => {
    urls.push(new URL(request.url).pathname);
  };
  server.events.on("request:start", onStart);
  try {
    const { router } = await renderApp("/acme/support-bot/spans");
    const list = await screen.findByRole("grid", { name: "Spans" });
    const row = within(list).getAllByRole("row")[1];
    if (!row) {
      throw new Error("missing span row");
    }
    fireEvent.click(row);
    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({ trace: TRACE, span: SPAN });
    });
    const tree = await screen.findByRole("grid", { name: "Trace" });
    expect(within(tree).getByRole("row", { selected: true })).toHaveTextContent(
      "chat claude-sonnet-4-5",
    );
    await waitFor(() => {
      expect(urls.some((path) => path.endsWith(`/spans/${SPAN}`))).toBe(true);
    });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("shows Spans after Traces in the nav", async () => {
  await renderApp("/acme/support-bot/spans");
  const spans = screen.getByRole("link", { name: "Spans" });
  const traces = screen.getByRole("link", { name: "Traces" });
  expect(spans).toHaveAttribute("aria-current", "page");
  expect(traces.compareDocumentPosition(spans) & Node.DOCUMENT_POSITION_FOLLOWING).toBe(
    Node.DOCUMENT_POSITION_FOLLOWING,
  );
});

it("has no default view to apply", async () => {
  const { router } = await renderApp("/acme/support-bot/spans");
  expect(await screen.findByRole("button", { name: "Default view" })).toBeInTheDocument();
  expect(router.state.location.search).toEqual({});
});

const QUEUE_LIST = "/api/v1/projects/:project/annotation-queues";
const QUEUE_ITEMS = "/api/v1/projects/:project/annotation-queues/:id/items";
const SPAN_QUEUE: Schema<"AnnotationQueue"> = {
  ...(queuesFixture.data[0] as Schema<"AnnotationQueue">),
  id: "01900000-0000-7000-8000-000000000a02",
  name: "Span review",
  target: "span",
};

/** The row checkbox of the `index`th loaded row. */
async function spanBox(index: number): Promise<HTMLElement> {
  const grid = await screen.findByRole("grid", { name: "Spans" });
  const box = within(grid).getAllByRole("checkbox", { name: "Select row" })[index];
  if (!box) {
    throw new Error(`no row checkbox ${index}`);
  }
  return box;
}

it("checks span rows without opening them and adds them to a span queue", async () => {
  server.use(
    http.get(QUEUE_LIST, () => HttpResponse.json({ data: [SPAN_QUEUE], next_cursor: null })),
  );
  const calls: { id: string; body: { items: { trace_id?: string; span_id?: string }[] } }[] = [];
  server.use(
    http.post(QUEUE_ITEMS, async ({ request, params }) => {
      const body = (await request.json()) as { items: { trace_id?: string; span_id?: string }[] };
      calls.push({ id: String(params.id), body });
      return HttpResponse.json(
        { added: body.items.length, existing: 0, skipped: [] },
        { status: 201 },
      );
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/spans");
  await user.click(await spanBox(0));
  await user.click(await spanBox(1));
  await user.click(screen.getByRole("button", { name: "Add 2 to queue" }));

  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
  await user.click(await screen.findByRole("option", { name: "Span review" }));
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(await within(dialog).findByText("Added 2 to Span review.")).toBeInTheDocument();
  expect(calls).toHaveLength(1);
  expect(calls[0]?.id).toBe(SPAN_QUEUE.id);
  expect(calls[0]?.body.items).toHaveLength(2);
  for (const item of calls[0]?.body.items ?? []) {
    expect(item.trace_id).toMatch(/^[0-9a-f]{32}$/);
    expect(item.span_id).toMatch(/^[0-9a-f]{16}$/);
  }
  expect(router.state.location.search.trace).toBeUndefined();
});

it("opens a span from its cell and not from its checkbox", async () => {
  const { router, user } = await renderApp("/acme/support-bot/spans");
  await user.click(await spanBox(0));
  expect(router.state.location.search.trace).toBeUndefined();

  const grid = await screen.findByRole("grid", { name: "Spans" });
  const row = within(grid).getAllByRole("row")[1];
  if (!row) {
    throw new Error("missing span row");
  }
  fireEvent.click(row);
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({ trace: TRACE, span: SPAN });
  });
});

it("a return within 15 minutes sends no list request", async () => {
  const { router } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("grid", { name: "Spans" });
  await leaveSpans(router);
  const recording = recordUrls();
  try {
    await returnToSpans(router);
    expect(screen.getByRole("grid", { name: "Spans" })).toHaveTextContent("get_order_status");
    expect(recording.urls.some(isListRequest)).toBe(false);
  } finally {
    recording.stop();
  }
});

it("the pill shows the new spans and loads them", async () => {
  server.use(
    http.get(SPANS_ENDPOINT, ({ request }) => {
      if (new URL(request.url).searchParams.get("limit") === "50") {
        return HttpResponse.json({
          ...spansFixture,
          data: spansFixture.data.slice(0, 2),
          next_cursor: null,
        });
      }
      return undefined;
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("grid", { name: "Spans" });
  await leaveSpans(router);
  await returnToSpans(router);
  const pill = await screen.findByRole("button", { name: "2 new spans ↑" });

  const recording = recordUrls();
  try {
    await user.click(pill);
    await waitFor(() => {
      expect(recording.urls.some(isListRequest)).toBe(true);
    });
    await waitFor(() => {
      expect(screen.getByRole("grid", { name: "Spans" })).not.toHaveAttribute("aria-busy");
    });
    expect(screen.queryByRole("button", { name: /new span/ })).not.toBeInTheDocument();
  } finally {
    recording.stop();
  }
});

it("no pill without new spans", async () => {
  server.use(
    http.get(SPANS_ENDPOINT, ({ request }) => {
      if (new URL(request.url).searchParams.get("limit") === "50") {
        return HttpResponse.json({ data: [], next_cursor: null });
      }
      return undefined;
    }),
  );
  const { router, queryClient } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("grid", { name: "Spans" });
  await leaveSpans(router);
  const recording = recordUrls();
  try {
    await returnToSpans(router);
    await waitFor(() => {
      expect(recording.urls.some(isCheckRequest)).toBe(true);
    });
    // The empty answer has landed once nothing is fetching.
    await waitFor(() => expect(queryClient.isFetching()).toBe(0));
    expect(screen.queryByRole("button", { name: /new span/ })).not.toBeInTheDocument();
  } finally {
    recording.stop();
  }
});

it("an expired anchor refetches", async () => {
  vi.useFakeTimers({ toFake: ["Date"], shouldAdvanceTime: true });
  try {
    const { router } = await renderApp("/acme/support-bot/spans");
    await screen.findByRole("grid", { name: "Spans" });
    await leaveSpans(router);
    vi.setSystemTime(Date.now() + 16 * 60_000);
    const recording = recordUrls();
    try {
      await returnToSpans(router);
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
    http.get(SPANS_ENDPOINT, async ({ request }) => {
      if (new URL(request.url).searchParams.has("filter")) {
        await new Promise(() => {});
      }
      return undefined;
    }),
  );
  const { router } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("grid", { name: "Spans" });
  await router.navigate({
    to: "/$org/$project/spans",
    params: SPANS_PATH,
    search: { filter: "status = 'error'" },
  });
  const grid = screen.getByRole("grid", { name: "Spans" });
  await waitFor(() => {
    expect(grid).toHaveAttribute("aria-busy", "true");
  });
  expect(grid).toHaveTextContent("get_order_status");
});

it("an absolute range sends no list and no check request", async () => {
  const from = "2026-01-01T09:00:00.000Z";
  const to = "2026-01-01T11:00:00.000Z";
  const { router } = await renderApp(`/acme/support-bot/spans?from=${from}&to=${to}`);
  await screen.findByRole("grid", { name: "Spans" });
  await router.navigate({ to: "/$org/$project/sessions", params: SPANS_PATH });
  const recording = recordUrls();
  try {
    await router.navigate({
      to: "/$org/$project/spans",
      params: SPANS_PATH,
      search: { from, to },
    });
    await screen.findByRole("grid", { name: "Spans" });
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(recording.urls.some(isListRequest)).toBe(false);
    expect(recording.urls.some(isCheckRequest)).toBe(false);
  } finally {
    recording.stop();
  }
});

it("refresh sets a new window", async () => {
  const { user } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("grid", { name: "Spans" });
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
