import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { ADA_ID, BOB_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import { ANNOTATION_DEFINITIONS, serveAnnotationDefinitions } from "@/test/score-definitions";
import { serveScores } from "@/test/scores";
import queueFixture from "../../../../../../../../fixtures/api/static/annotation-queue.json";
import itemFixture from "../../../../../../../../fixtures/api/static/annotation-queue-item.json";
import projectFixture from "../../../../../../../../fixtures/api/static/project.json";

const QUEUE = "01900000-0000-7000-8000-000000000a01";
const ITEM = "01900000-0000-7000-8000-000000000a11";
const NEXT_ITEM = "01900000-0000-7000-8000-000000000a12";
const SESSION = "sess-001";
const HELPFULNESS = "01900000-0000-7000-8000-000000000601";
const TONE = "01900000-0000-7000-8000-000000000603";
const PAGE = `/acme/support-bot/annotation-queues/${QUEUE}/review`;
const ITEM_URL = `${PAGE}/${ITEM}`;
const NEXT_URL = `${PAGE}/next`;

type Item = Schema<"AnnotationQueueItem">;
type Queue = Schema<"AnnotationQueue">;

const queue: Queue = {
  ...(queueFixture as Queue),
  definition_ids: [HELPFULNESS, TONE],
};
const pendingItem = itemFixture as Item;

afterEach(() => {
  vi.useRealTimers();
});

/** The queue and the item the reviewer loads, and a lease that the caller takes. */
function serveReview({
  item = pendingItem,
  target = "trace" as Queue["target"],
  lease = 200,
}: {
  item?: Item;
  target?: Queue["target"];
  lease?: number;
} = {}) {
  server.use(
    http.get("/api/v1/projects/:project/annotation-queues/:id", () =>
      HttpResponse.json({ ...queue, target }),
    ),
    http.get("/api/v1/projects/:project/annotation-queues/:id/items/:item_id", () =>
      HttpResponse.json(item),
    ),
    http.post("/api/v1/projects/:project/annotation-queues/:id/items/:item_id/lease", () =>
      lease === 200
        ? HttpResponse.json(item)
        : problemResponse(409, "lease_held", {
            detail: "The item is leased by another reviewer.",
            errors: [{ field: "leased_by_user_id", message: BOB_ID }],
          }),
    ),
    http.delete(
      "/api/v1/projects/:project/annotation-queues/:id/items/:item_id/lease",
      () => new HttpResponse(null, { status: 204 }),
    ),
  );
}

function serveWrites(
  overrides: { scores?: () => Response; complete?: () => Response; next?: () => Response } = {},
) {
  server.use(
    http.post("/api/v1/projects/:project/scores", () =>
      overrides.scores ? overrides.scores() : HttpResponse.json({ data: [] }, { status: 201 }),
    ),
    http.post("/api/v1/projects/:project/annotation-queues/:id/items/:item_id/complete", () =>
      overrides.complete
        ? overrides.complete()
        : HttpResponse.json({ ...pendingItem, status: "completed", done_by_user_id: ADA_ID }),
    ),
    http.post("/api/v1/projects/:project/annotation-queues/:id/next", () =>
      overrides.next ? overrides.next() : HttpResponse.json({ ...pendingItem, id: NEXT_ITEM }),
    ),
    http.post("/api/v1/projects/:project/annotation-queues/:id/items/:item_id/skip", () =>
      HttpResponse.json({ ...pendingItem, status: "skipped" }),
    ),
    http.post("/api/v1/projects/:project/annotation-queues/:id/items/:item_id/reopen", () =>
      HttpResponse.json({ ...pendingItem, status: "pending" }),
    ),
    http.delete(
      "/api/v1/projects/:project/annotation-queues/:id/items/:item_id",
      () => new HttpResponse(null, { status: 204 }),
    ),
  );
}

function serveViewer() {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
}

it("next resolves to an item and claims once", async () => {
  serveReview();
  serveWrites();
  const requests = recordRequests();
  const { router } = await renderApp(NEXT_URL);
  await waitFor(() => {
    expect(router.state.location.pathname).toBe(`${PAGE}/${NEXT_ITEM}`);
  });
  const claims = requests.calls.filter(
    (call) => call.method === "POST" && call.path.endsWith("/next"),
  );
  expect(claims).toHaveLength(1);
  requests.stop();
});

it("next with nothing free shows the end screen, and check again claims again", async () => {
  serveReview();
  serveWrites({ next: () => new HttpResponse(null, { status: 204 }) });
  const requests = recordRequests();
  const { user } = await renderApp(NEXT_URL);
  expect(await screen.findByText("Nothing left to review")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Check again" }));
  await waitFor(() => {
    const claims = requests.calls.filter((call) => call.path.endsWith("/next"));
    expect(claims).toHaveLength(2);
  });
  requests.stop();
});

it("a viewer on next sends nothing and sees the note", async () => {
  serveViewer();
  serveReview();
  serveWrites();
  const requests = recordRequests();
  await renderApp(NEXT_URL);
  expect(await screen.findByText("Only members can review.")).toBeInTheDocument();
  expect(requests.calls.some((call) => call.path.endsWith("/next"))).toBe(false);
  requests.stop();
});

it("a trace item shows the trace, the instructions and the queue's definitions in order", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  await renderApp(ITEM_URL);
  expect(await screen.findByText("Instructions")).toBeInTheDocument();
  const form = await screen.findByRole("form", { name: "Annotations" });
  const labels = within(form)
    .getAllByRole("spinbutton")
    .map((control) => control.getAttribute("aria-label"));
  expect(labels).toEqual(["helpfulness"]);
});

it("a session item shows the conversation", async () => {
  serveReview({
    target: "session",
    item: { ...pendingItem, trace_id: undefined, session_id: SESSION },
  });
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  await renderApp(ITEM_URL);
  expect(await screen.findByRole("region", { name: "Conversation" })).toBeInTheDocument();
});

it("complete and next saves, completes, then claims the next", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  const { user } = await renderApp(ITEM_URL);
  const helpfulness = await screen.findByRole("spinbutton", { name: "helpfulness" });
  fireEvent.change(helpfulness, { target: { value: "0.5" } });
  await user.click(screen.getByRole("button", { name: "Complete and next" }));
  await waitFor(() => {
    const writes = requests.calls
      .filter((call) => call.method === "POST" && !call.path.endsWith("/lease"))
      .map((call) => call.path.replace(/^.*\/api\/v1\/projects\/[^/]+/, ""));
    expect(writes).toEqual([
      "/scores",
      `/annotation-queues/${QUEUE}/items/${ITEM}/complete`,
      `/annotation-queues/${QUEUE}/next`,
    ]);
  });
  await waitFor(() => {
    const save = requests.calls.find(
      (call) => call.method === "POST" && call.path.endsWith("/scores"),
    );
    expect(save?.body).toMatchObject({ scores: [{ queue_id: QUEUE }] });
  });
  requests.stop();
});

it("a failed save completes nothing", async () => {
  serveReview();
  serveWrites({ scores: () => problemResponse(500, "internal", { detail: "The save failed." }) });
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  const { user } = await renderApp(ITEM_URL);
  fireEvent.change(await screen.findByRole("spinbutton", { name: "helpfulness" }), {
    target: { value: "0.5" },
  });
  await user.click(screen.getByRole("button", { name: "Complete and next" }));
  expect(await screen.findByText("The save failed.")).toBeInTheDocument();
  expect(requests.calls.some((call) => call.path.endsWith("/complete"))).toBe(false);
  requests.stop();
});

it("ctrl+enter completes and next", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  const { user } = await renderApp(ITEM_URL);
  await screen.findByRole("form", { name: "Annotations" });
  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Complete and next" })).toBeEnabled();
  });
  await user.keyboard("{Control>}{Enter}{/Control}");
  await waitFor(() => {
    expect(requests.calls.some((call) => call.path.endsWith("/complete"))).toBe(true);
  });
  requests.stop();
});

it("ctrl+shift+enter moves on without completing", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  const { user } = await renderApp(ITEM_URL);
  await screen.findByRole("form", { name: "Annotations" });
  await user.keyboard("{Control>}{Shift>}{Enter}{/Shift}{/Control}");
  await waitFor(() => {
    expect(requests.calls.some((call) => call.path.endsWith("/next"))).toBe(true);
  });
  expect(requests.calls.some((call) => call.path.endsWith("/complete"))).toBe(false);
  requests.stop();
});

it("skip sends the reason and moves on", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  const { user } = await renderApp(ITEM_URL);
  await user.click(await screen.findByRole("button", { name: "Skip…" }));
  await user.type(screen.getByRole("textbox", { name: "Reason (optional)" }), "Off topic");
  await user.click(screen.getByRole("button", { name: "Skip and next" }));
  await waitFor(() => {
    const skip = requests.calls.find((call) => call.path.endsWith("/skip"));
    expect(skip?.body).toEqual({ reason: "Off topic" });
  });
  await waitFor(() => {
    expect(requests.calls.some((call) => call.path.endsWith("/next"))).toBe(true);
  });
  requests.stop();
});

it("a lease conflict hides the form", async () => {
  serveReview({ lease: 409 });
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  await renderApp(ITEM_URL);
  expect(await screen.findByText("Another reviewer took this item.")).toBeInTheDocument();
  expect(screen.queryByRole("form", { name: "Annotations" })).not.toBeInTheDocument();
});

it("the heartbeat renews the lease", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  // The interval is created on mount, so the fake clock must be installed before rendering.
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
  await renderApp(ITEM_URL);
  await screen.findByRole("form", { name: "Annotations" });
  const leases = () =>
    requests.calls.filter((call) => call.method === "POST" && call.path.endsWith("/lease"));
  await waitFor(() => {
    expect(leases()).toHaveLength(1);
  });
  act(() => {
    vi.advanceTimersByTime(60_000);
  });
  await waitFor(() => {
    expect(leases()).toHaveLength(2);
  });
  requests.stop();
});

it("leaving releases the lease", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  const { unmount } = await renderApp(ITEM_URL);
  await screen.findByRole("form", { name: "Annotations" });
  unmount();
  await waitFor(() => {
    expect(
      requests.calls.some((call) => call.method === "DELETE" && call.path.endsWith("/lease")),
    ).toBe(true);
  });
  requests.stop();
});

it("a viewer sees no form and sends no lease", async () => {
  serveViewer();
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  await renderApp(ITEM_URL);
  expect(await screen.findByText("Only members can review.")).toBeInTheDocument();
  expect(screen.queryByRole("form", { name: "Annotations" })).not.toBeInTheDocument();
  expect(requests.calls.some((call) => call.path.endsWith("/lease"))).toBe(false);
  requests.stop();
});

it("links back to the queue", async () => {
  serveReview();
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  await renderApp(ITEM_URL);
  const back = await screen.findByRole("link", { name: `Back to ${queue.name}` });
  expect(back.getAttribute("href")).toMatch(new RegExp(`/annotation-queues/${QUEUE}$`));
});

it("a deleted trace shows the not-available state, and Remove moves on", async () => {
  serveReview();
  serveWrites();
  server.use(
    http.get("/api/v1/projects/:project/traces/:trace_id", () =>
      problemResponse(404, "not_found", { detail: "No such trace." }),
    ),
  );
  const requests = recordRequests();
  const { user } = await renderApp(ITEM_URL);
  expect(await screen.findByText("This trace is no longer available")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Remove from queue" }));
  await waitFor(() => {
    expect(
      requests.calls.some((call) => call.method === "DELETE" && call.path.endsWith(`/${ITEM}`)),
    ).toBe(true);
  });
  requests.stop();
});

it("a completed item shows who completed it, the form, and Reopen", async () => {
  serveReview({
    item: {
      ...pendingItem,
      status: "completed",
      done_by_user_id: ADA_ID,
      done_at: "2026-01-01T10:00:00.000000000Z",
      leased_by_user_id: undefined,
      lease_expires_at: undefined,
    },
  });
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  await renderApp(ITEM_URL);
  expect(await screen.findByText(/Completed by/)).toBeInTheDocument();
  expect(await screen.findByRole("form", { name: "Annotations" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Reopen" })).toBeInTheDocument();
  expect(requests.calls.some((call) => call.path.endsWith("/lease"))).toBe(false);
  requests.stop();
});

it("saving a completed item sends its scores and does not complete it", async () => {
  serveReview({
    item: {
      ...pendingItem,
      status: "completed",
      done_by_user_id: ADA_ID,
      done_at: "2026-01-01T10:00:00.000000000Z",
      leased_by_user_id: undefined,
      lease_expires_at: undefined,
    },
  });
  serveWrites();
  serveAnnotationDefinitions(ANNOTATION_DEFINITIONS);
  serveScores([]);
  const requests = recordRequests();
  const { user } = await renderApp(ITEM_URL);
  fireEvent.change(await screen.findByRole("spinbutton", { name: "helpfulness" }), {
    target: { value: "0.5" },
  });
  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
  });
  await user.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => {
    const save = requests.calls.find(
      (call) => call.method === "POST" && call.path.endsWith("/scores"),
    );
    expect(save?.body).toMatchObject({ scores: [{ queue_id: QUEUE }] });
  });
  expect(requests.calls.some((call) => call.path.endsWith("/complete"))).toBe(false);
  requests.stop();
});
