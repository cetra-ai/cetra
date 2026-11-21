import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import { recordUrls } from "@/test/requests";
import sessionsFixture from "../../../../../../fixtures/api/responses/sessions.json";
import queuesFixture from "../../../../../../fixtures/api/static/annotation-queues.json";

const SESSIONS_ENDPOINT = "*/api/v1/projects/:project/sessions";
const SESSIONS_PATH = { org: "acme", project: "support-bot" } as const;

type Router = Awaited<ReturnType<typeof renderApp>>["router"];

/** A list request: the first page of the window (limit 100). */
function isListRequest(url: URL): boolean {
  return url.pathname.endsWith("/sessions") && url.searchParams.get("limit") === "100";
}

/** A new-rows check request (limit 50). */
function isCheckRequest(url: URL): boolean {
  return url.pathname.endsWith("/sessions") && url.searchParams.get("limit") === "50";
}

async function leaveSessions(router: Router) {
  await router.navigate({ to: "/$org/$project/users", params: SESSIONS_PATH });
}

async function returnToSessions(router: Router) {
  await router.navigate({ to: "/$org/$project/sessions", params: SESSIONS_PATH });
  await screen.findByRole("grid", { name: "Sessions" });
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
  const { router } = await renderApp("/acme/support-bot/sessions");
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Sessions"]);
  const grid = await screen.findByRole("grid", { name: "Sessions" });
  stubHeaderSlots(grid);
  vi.useFakeTimers();
  try {
    // Start is the second column; drop it on the Session slot.
    fireEvent.pointerDown(screen.getByRole("columnheader", { name: "Start" }), {
      button: 0,
      clientX: 350,
      clientY: 10,
    });
    fireEvent.pointerMove(window, { clientX: 20, clientY: 10 });
    fireEvent.pointerUp(window, { clientX: 20, clientY: 10 });
  } finally {
    vi.useRealTimers();
  }
  await waitFor(() => {
    expect(
      String(router.state.location.search.cols ?? "")
        .split(",")
        .slice(0, 3),
    ).toEqual(["start", "session", "first_message"]);
  });
});

it("renders the session fixture", async () => {
  await renderApp("/acme/support-bot/sessions");
  const grid = await screen.findByRole("grid", { name: "Sessions" });
  expect(within(grid).getByText("sess-001")).toBeInTheDocument();
  expect(within(grid).getByText("sess-002")).toBeInTheDocument();
  expect(within(grid).getByText("user-42")).toBeInTheDocument();
  expect(within(grid).getByText("user-7")).toBeInTheDocument();
  expect(within(grid).getByRole("columnheader", { name: "resolved" })).toBeInTheDocument();
  expect(within(grid).queryByRole("columnheader", { name: "Tags" })).not.toBeInTheDocument();
  expect(within(grid).getByRole("columnheader", { name: "Start" })).toHaveAttribute(
    "aria-sort",
    "descending",
  );
  const row = within(grid).getByRole("row", { name: /sess-002/ });
  expect(within(row).getByText("2")).toHaveClass("text-destructive");
  expect(screen.queryByRole("searchbox", { name: "Search" })).not.toBeInTheDocument();
});

it("uses the include-eval fixture when eval is on", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/sessions")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { user } = await renderApp("/acme/support-bot/sessions");
    const grid = await screen.findByRole("grid", { name: "Sessions" });
    expect(within(grid).queryByText("3")).not.toBeInTheDocument();
    await user.click(
      screen.getByRole("button", { name: /^Include evaluation and experiment traces/ }),
    );
    await user.click(await screen.findByRole("menuitemcheckbox", { name: "Evaluation traces" }));
    expect(await screen.findByText("3")).toBeInTheDocument();
    expect(urls.some((url) => url.searchParams.get("include_eval") === "true")).toBe(true);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("sorts by cost with sort=cost_total", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/sessions")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { router } = await renderApp("/acme/support-bot/sessions");
    const grid = await screen.findByRole("grid", { name: "Sessions" });
    fireEvent.click(within(grid).getByRole("button", { name: "Cost" }));
    await waitFor(() => {
      expect(urls.some((url) => url.searchParams.get("sort") === "cost_total")).toBe(true);
    });
    expect(router.state.location.search).toMatchObject({ sort: "cost_total", order: "desc" });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("shows the summary strip and the sessions-started chart above the table", async () => {
  await renderApp("/acme/support-bot/sessions");
  const grid = await screen.findByRole("grid", { name: "Sessions" });
  const summary = screen.getByRole("region", { name: "Summary" });
  const chart = screen.getByRole("img", { name: "Sessions started" });
  const sessionRows = within(grid)
    .getAllByRole("row")
    .filter((row) => /sess-/.test(row.textContent ?? ""));
  expect(within(summary).getByText(String(sessionRows.length))).toBeInTheDocument();
  expect(summary.compareDocumentPosition(grid) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(chart.compareDocumentPosition(grid) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
});

it("restores the list on the browser's Back button too, with the opened row highlighted", async () => {
  const { router } = await renderApp("/acme/support-bot/sessions?sort=cost_total&order=asc");
  const grid = await screen.findByRole("grid", { name: "Sessions" });
  fireEvent.click(within(grid).getByRole("row", { name: /sess-002/ }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions/sess-002");
  });
  act(() => {
    router.history.back();
  });
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions");
  });
  expect(router.state.location.search).toMatchObject({ sort: "cost_total", order: "asc" });
  expect(router.state.location.state.listRestore).toMatchObject({ openedId: "sess-002" });
  const restored = await screen.findByRole("grid", { name: "Sessions" });
  await waitFor(() => {
    expect(within(restored).getByRole("row", { name: /sess-002/ })).toHaveClass("live-row");
  });
});

it("returns from a session to the same list search, with the opened row highlighted", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sessions?sort=cost_total&order=asc");
  const grid = await screen.findByRole("grid", { name: "Sessions" });
  fireEvent.click(within(grid).getByRole("row", { name: /sess-002/ }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions/sess-002");
  });
  await user.click(await screen.findByRole("link", { name: "Back to sessions" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions");
  });
  expect(router.state.location.search).toMatchObject({ sort: "cost_total", order: "asc" });
  const restored = await screen.findByRole("grid", { name: "Sessions" });
  await waitFor(() => {
    expect(within(restored).getByRole("row", { name: /sess-002/ })).toHaveClass("live-row");
  });
});

const QUEUE_LIST = "/api/v1/projects/:project/annotation-queues";
const QUEUE_ITEMS = "/api/v1/projects/:project/annotation-queues/:id/items";
const SESSION_QUEUE: Schema<"AnnotationQueue"> = {
  ...(queuesFixture.data[0] as Schema<"AnnotationQueue">),
  id: "01900000-0000-7000-8000-000000000a03",
  name: "Session review",
  target: "session",
};

/** The row checkbox of the `index`th loaded row. */
async function sessionBox(index: number): Promise<HTMLElement> {
  const grid = await screen.findByRole("grid", { name: "Sessions" });
  const box = within(grid).getAllByRole("checkbox", { name: "Select row" })[index];
  if (!box) {
    throw new Error(`no row checkbox ${index}`);
  }
  return box;
}

it("checks session rows without opening them and adds them to a session queue", async () => {
  server.use(
    http.get(QUEUE_LIST, () => HttpResponse.json({ data: [SESSION_QUEUE], next_cursor: null })),
  );
  const calls: { id: string; body: { items: { session_id?: string }[] } }[] = [];
  server.use(
    http.post(QUEUE_ITEMS, async ({ request, params }) => {
      const body = (await request.json()) as { items: { session_id?: string }[] };
      calls.push({ id: String(params.id), body });
      return HttpResponse.json(
        { added: body.items.length, existing: 0, skipped: [] },
        { status: 201 },
      );
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/sessions");
  await user.click(await sessionBox(0));
  await user.click(await sessionBox(1));
  await user.click(screen.getByRole("button", { name: "Add 2 to queue" }));

  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
  await user.click(await screen.findByRole("option", { name: "Session review" }));
  await user.click(within(dialog).getByRole("button", { name: "Add" }));

  expect(await within(dialog).findByText("Added 2 to Session review.")).toBeInTheDocument();
  expect(calls).toHaveLength(1);
  expect(calls[0]?.id).toBe(SESSION_QUEUE.id);
  expect(calls[0]?.body.items).toHaveLength(2);
  for (const item of calls[0]?.body.items ?? []) {
    expect(item.session_id).toMatch(/^sess-/);
  }
  expect(router.state.location.pathname).toBe("/acme/support-bot/sessions");
});

it("a return within 15 minutes sends no list request", async () => {
  const { router } = await renderApp("/acme/support-bot/sessions");
  await screen.findByRole("grid", { name: "Sessions" });
  await leaveSessions(router);
  const recording = recordUrls();
  try {
    await returnToSessions(router);
    expect(screen.getByRole("grid", { name: "Sessions" })).toHaveTextContent("sess-001");
    expect(recording.urls.some(isListRequest)).toBe(false);
  } finally {
    recording.stop();
  }
});

it("the pill shows the new sessions and loads them", async () => {
  server.use(
    http.get(SESSIONS_ENDPOINT, ({ request }) => {
      if (new URL(request.url).searchParams.get("limit") === "50") {
        return HttpResponse.json({
          ...sessionsFixture,
          data: sessionsFixture.data.slice(0, 2),
          next_cursor: null,
        });
      }
      return undefined;
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/sessions");
  await screen.findByRole("grid", { name: "Sessions" });
  await leaveSessions(router);
  await returnToSessions(router);
  const pill = await screen.findByRole("button", { name: "2 sessions with new activity ↑" });

  const recording = recordUrls();
  try {
    await user.click(pill);
    await waitFor(() => {
      expect(recording.urls.some(isListRequest)).toBe(true);
    });
    await waitFor(() => {
      expect(screen.getByRole("grid", { name: "Sessions" })).not.toHaveAttribute("aria-busy");
    });
    expect(screen.queryByRole("button", { name: /with new activity/ })).not.toBeInTheDocument();
  } finally {
    recording.stop();
  }
});

it("an expired anchor refetches", async () => {
  vi.useFakeTimers({ toFake: ["Date"], shouldAdvanceTime: true });
  try {
    const { router } = await renderApp("/acme/support-bot/sessions");
    await screen.findByRole("grid", { name: "Sessions" });
    await leaveSessions(router);
    vi.setSystemTime(Date.now() + 16 * 60_000);
    const recording = recordUrls();
    try {
      await returnToSessions(router);
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
    http.get(SESSIONS_ENDPOINT, async ({ request }) => {
      if (new URL(request.url).searchParams.has("filter")) {
        await new Promise(() => {});
      }
      return undefined;
    }),
  );
  const { router } = await renderApp("/acme/support-bot/sessions");
  await screen.findByRole("grid", { name: "Sessions" });
  await router.navigate({
    to: "/$org/$project/sessions",
    params: SESSIONS_PATH,
    search: { filter: "status = 'error'" },
  });
  const grid = screen.getByRole("grid", { name: "Sessions" });
  await waitFor(() => {
    expect(grid).toHaveAttribute("aria-busy", "true");
  });
  expect(grid).toHaveTextContent("sess-001");
});
