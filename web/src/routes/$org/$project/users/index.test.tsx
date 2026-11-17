import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

function topUser(
  userId: string,
  costTotal: number,
  metadata?: Record<string, string>,
): Schema<"EndUserSummary"> {
  return {
    user_id: userId,
    first_seen: "2026-01-01T10:00:00.000000000Z",
    last_seen: "2026-01-01T10:20:03.000000000Z",
    trace_count: 4,
    session_count: 1,
    error_count: 0,
    error_trace_count: 0,
    cost_total: costTotal,
    activity: Array.from({ length: 24 }, () => 0),
    scores: [],
    metadata,
  };
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
  within(grid)
    .getAllByRole("columnheader")
    .forEach((header, index) => {
      header.getBoundingClientRect = () => slotRect(index * 100, 100);
    });
  const scroller = grid.querySelector<HTMLElement>("[data-scroll-container]");
  if (scroller) {
    scroller.getBoundingClientRect = () => slotRect(0, 1000);
  }
}

it("saves a column dragged to a new place as cols", async () => {
  const { router } = await renderApp("/acme/support-bot/users");
  const grid = await screen.findByRole("grid", { name: "Users" });
  stubHeaderSlots(grid);
  vi.useFakeTimers();
  try {
    // Activity is the second column; drop it on the User slot.
    fireEvent.pointerDown(screen.getByRole("columnheader", { name: "Activity" }), {
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
    ).toEqual(["activity", "user", "traces"]);
  });
});

it("renders the user fixture sorted by last seen", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/users")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    await renderApp("/acme/support-bot/users");
    const grid = await screen.findByRole("grid", { name: "Users" });
    expect(within(grid).getByText("user-42")).toBeInTheDocument();
    expect(within(grid).getByText("user-7")).toBeInTheDocument();
    expect(within(grid).getByRole("columnheader", { name: "Last seen" })).toHaveAttribute(
      "aria-sort",
      "descending",
    );
    expect(within(grid).getByText("2")).toHaveClass("text-destructive");
    expect(screen.getByRole("button", { name: "Edit filter" })).toHaveTextContent(
      "Filter traces, e.g. environment = 'production'",
    );
    expect(screen.queryByRole("searchbox", { name: "Search" })).not.toBeInTheDocument();
    // The top-users row (UP-7) always sorts by cost; the list itself sends no sort.
    expect(
      urls.some(
        (url) => url.searchParams.has("sort") && url.searchParams.get("sort") !== "cost_total",
      ),
    ).toBe(false);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("sends the filter to list_users", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/users")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const filter = "environment = 'production'";
    await renderApp(`/acme/support-bot/users?filter=${encodeURIComponent(filter)}`);
    await screen.findByRole("grid", { name: "Users" });
    expect(urls.some((url) => url.searchParams.get("filter") === filter)).toBe(true);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("shows the summary strip with the metrics traces KPI", async () => {
  await renderApp("/acme/support-bot/users");
  await screen.findByRole("grid", { name: "Users" });
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("Users")).toBeInTheDocument();
  await waitFor(() => {
    expect(within(summary).getByText("Traces").parentElement).toHaveTextContent("Traces 5");
  });
});

it("shows the activity and error rate columns", async () => {
  await renderApp("/acme/support-bot/users");
  const grid = await screen.findByRole("grid", { name: "Users" });
  expect(within(grid).getByRole("columnheader", { name: "Activity" })).toBeInTheDocument();
  expect(within(grid).getByRole("columnheader", { name: "Error rate" })).toBeInTheDocument();
});

it("sorts by tokens when the Tokens header is clicked", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/users")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { router, user } = await renderApp("/acme/support-bot/users");
    const grid = await screen.findByRole("grid", { name: "Users" });
    await user.click(within(grid).getByRole("button", { name: "Tokens" }));
    await waitFor(() => {
      expect(router.state.location.search.sort).toBe("total_tokens");
    });
    await waitFor(() => {
      expect(
        urls.some(
          (url) =>
            url.searchParams.get("sort") === "total_tokens" &&
            url.searchParams.get("order") === "desc",
        ),
      ).toBe(true);
    });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("opens the user panel when a row is clicked, and Escape closes it", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users");
  const grid = await screen.findByRole("grid", { name: "Users" });
  await user.click(within(grid).getByText("user-42"));
  await waitFor(() => {
    expect(router.state.location.search.user).toBe("user-42");
  });
  const panel = await screen.findByRole("region", { name: "User" });
  expect(within(panel).getByRole("link", { name: "Open page" })).toHaveAttribute(
    "href",
    "/acme/support-bot/users/user-42",
  );
  fireEvent.keyDown(window, { key: "Escape" });
  await waitFor(() => {
    expect(router.state.location.search.user).toBeUndefined();
  });
  await waitFor(() => {
    expect(screen.queryByRole("region", { name: "User" })).not.toBeInTheDocument();
  });
});

it("shows the top users by cost under the summary, and opens the drawer on a click", async () => {
  const top = [topUser("user-top-a", 2, { name: '"Grace Test"' }), topUser("user-top-b", 1)];
  server.use(
    http.get("/api/v1/projects/:project/users", ({ request }) => {
      if (new URL(request.url).searchParams.get("sort") !== "cost_total") {
        return undefined;
      }
      return HttpResponse.json({ data: top, next_cursor: null });
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/users");
  const section = await screen.findByRole("region", { name: "Top users by cost" });
  expect(
    within(section)
      .getAllByRole("button")
      .map((button) => button.getAttribute("aria-label")),
  ).toEqual([`Grace Test: ${formatCost(2)}`, `user-top-b: ${formatCost(1)}`]);
  await user.click(within(section).getByRole("button", { name: `user-top-b: ${formatCost(1)}` }));
  await waitFor(() => {
    expect(router.state.location.search.user).toBe("user-top-b");
  });
});

it("sends the top-users request with the list's range, filter and flags", async () => {
  const listUrls: URL[] = [];
  const topUrls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/users")) {
      listUrls.push(url);
    }
  };
  server.use(
    http.get("/api/v1/projects/:project/users", ({ request }) => {
      if (new URL(request.url).searchParams.get("sort") === "cost_total") {
        topUrls.push(new URL(request.url));
      }
      return undefined;
    }),
  );
  server.events.on("request:start", onStart);
  try {
    const filter = "status = 'ok'";
    await renderApp(
      `/acme/support-bot/users?range=7d&eval=1&exp=1&filter=${encodeURIComponent(filter)}`,
    );
    await screen.findByRole("grid", { name: "Users" });
    await waitFor(() => {
      expect(topUrls.length).toBeGreaterThan(0);
    });
    const list = listUrls.find((url) => !url.searchParams.has("sort"));
    const top = topUrls[0];
    expect(list).toBeDefined();
    expect(top?.searchParams.get("from")).toBe(list?.searchParams.get("from"));
    expect(top?.searchParams.get("to")).toBe(list?.searchParams.get("to"));
    expect(top?.searchParams.get("filter")).toBe(filter);
    expect(top?.searchParams.get("include_eval")).toBe("true");
    expect(top?.searchParams.get("include_experiments")).toBe("true");
    expect(top?.searchParams.get("limit")).toBe("5");
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});
