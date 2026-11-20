import { screen, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { addToQueueReason } from "@/components/annotation-queues/add-to-queue-dialog";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import projectFixture from "../../../../fixtures/api/static/project.json";
import queuesFixture from "../../../../fixtures/api/static/annotation-queues.json";

const TRACE = "a1000000000000000000000000000001";
const SPAN = "b100000000000014";
const SESSION = "sess-001";
const LIST = "/api/v1/projects/:project/annotation-queues";
const ITEMS = "/api/v1/projects/:project/annotation-queues/:id/items";

type Queue = Schema<"AnnotationQueue">;
const QUEUE: Queue = queuesFixture.data[0] as Queue;
const SPAN_QUEUE: Queue = {
  ...QUEUE,
  id: "01900000-0000-7000-8000-000000000a02",
  name: "Span review",
  target: "span",
};
const SESSION_QUEUE: Queue = {
  ...QUEUE,
  id: "01900000-0000-7000-8000-000000000a03",
  name: "Session review",
  target: "session",
};

const TRACE_PAGE = `/acme/support-bot/traces/${TRACE}`;
const SPAN_URL = `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}&span=${SPAN}`;
const SESSION_PAGE = `/acme/support-bot/sessions/${SESSION}`;

/** Serves these queues for every list request. */
function serveQueues(queues: Queue[]) {
  server.use(http.get(LIST, () => HttpResponse.json({ data: queues, next_cursor: null })));
}

/** Answers each add with `answer` and records the queue id and the body of each request. */
function captureAdds(
  answer: Schema<"AddedQueueItems"> = { added: 1, existing: 0, skipped: [] },
  status = 201,
) {
  const calls: { id: string; body: unknown }[] = [];
  server.use(
    http.post(ITEMS, async ({ request, params }) => {
      calls.push({ id: String(params.id), body: await request.json() });
      return HttpResponse.json(answer, { status });
    }),
  );
  return calls;
}

/** Picks an option of the queue select inside the dialog. */
async function choose(user: UserEvent, dialog: HTMLElement, option: string) {
  await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
  await user.click(await screen.findByRole("option", { name: option }));
}

describe("add to queue reason", () => {
  it("says why a viewer, or too many items, cannot be added", () => {
    expect(addToQueueReason("viewer", 1)).toBe("Only members can add to queues");
    expect(addToQueueReason("member", 1_001)).toBe("Select at most 1 000 rows");
    expect(addToQueueReason("member", 1_000)).toBeUndefined();
    expect(addToQueueReason("admin", 1)).toBeUndefined();
  });
});

describe("add to queue from the trace page", () => {
  it("adds the trace to a trace queue and shows the result", async () => {
    serveQueues([QUEUE]);
    const calls = captureAdds();
    const { user } = await renderApp(TRACE_PAGE);
    const header = await screen.findByRole("region", { name: "Trace" });
    await user.click(within(header).getByRole("button", { name: "Add to queue" }));
    const dialog = await screen.findByRole("dialog");
    await choose(user, dialog, "Relevance review");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Added 1 to Relevance review.")).toBeInTheDocument();
    expect(calls).toEqual([{ id: QUEUE.id, body: { items: [{ trace_id: TRACE }] } }]);
    expect(within(dialog).getByRole("link", { name: "Open queue" })).toHaveAttribute(
      "href",
      `/acme/support-bot/annotation-queues/${QUEUE.id}`,
    );
  });

  it("lists only the queues of the selected span's kind", async () => {
    serveQueues([QUEUE, SPAN_QUEUE]);
    const { user } = await renderApp(SPAN_URL);
    const header = await screen.findByRole("region", { name: "Trace" });
    await user.click(within(header).getByRole("button", { name: "Add to queue" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));

    expect(await screen.findByRole("option", { name: "Span review" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "Relevance review" })).not.toBeInTheDocument();
  });

  it("adds a selected child span as a span target", async () => {
    serveQueues([QUEUE, SPAN_QUEUE]);
    const calls = captureAdds();
    const { user } = await renderApp(SPAN_URL);
    const header = await screen.findByRole("region", { name: "Trace" });
    await user.click(within(header).getByRole("button", { name: "Add to queue" }));
    const dialog = await screen.findByRole("dialog");
    await choose(user, dialog, "Span review");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Added 1 to Span review.")).toBeInTheDocument();
    expect(calls).toEqual([
      { id: SPAN_QUEUE.id, body: { items: [{ trace_id: TRACE, span_id: SPAN }] } },
    ]);
  });

  it("opens the new queue form with the kind of the item", async () => {
    serveQueues([QUEUE]);
    const { user } = await renderApp(SPAN_URL);
    const header = await screen.findByRole("region", { name: "Trace" });
    await user.click(within(header).getByRole("button", { name: "Add to queue" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
    await user.click(await screen.findByRole("option", { name: "New queue…" }));

    const create = await screen.findByRole("dialog", { name: "New annotation queue" });
    expect(within(create).getByRole("radio", { name: "Spans" })).toBeChecked();
  });

  it("explains a full queue", async () => {
    serveQueues([QUEUE]);
    server.use(http.post(ITEMS, () => problemResponse(422, "too_many_items")));
    const { user } = await renderApp(TRACE_PAGE);
    const header = await screen.findByRole("region", { name: "Trace" });
    await user.click(within(header).getByRole("button", { name: "Add to queue" }));
    const dialog = await screen.findByRole("dialog");
    await choose(user, dialog, "Relevance review");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(
      await within(dialog).findByText("The queue is full (100 000 items)."),
    ).toBeInTheDocument();
  });

  it("names the items that were not added and why", async () => {
    serveQueues([QUEUE]);
    captureAdds({
      added: 0,
      existing: 0,
      skipped: [{ index: 0, message: "The trace is not visible" }],
    });
    const { user } = await renderApp(TRACE_PAGE);
    const header = await screen.findByRole("region", { name: "Trace" });
    await user.click(within(header).getByRole("button", { name: "Add to queue" }));
    const dialog = await screen.findByRole("dialog");
    await choose(user, dialog, "Relevance review");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("The trace is not visible")).toBeInTheDocument();
    expect(within(dialog).getByText("1 not added")).toBeInTheDocument();
  });

  it("disables the button for a viewer", async () => {
    server.use(
      http.get("/api/v1/projects/:project", () =>
        HttpResponse.json({ ...projectFixture, role: "viewer" }),
      ),
    );
    await renderApp(TRACE_PAGE);
    const header = await screen.findByRole("region", { name: "Trace" });
    expect(within(header).getByRole("button", { name: "Add to queue" })).toBeDisabled();
  });
});

describe("add to queue from the session page", () => {
  it("adds the session to a session queue", async () => {
    serveQueues([SESSION_QUEUE]);
    const calls = captureAdds();
    const { user } = await renderApp(SESSION_PAGE);
    await user.click(await screen.findByRole("button", { name: "Add to queue" }));
    const dialog = await screen.findByRole("dialog");
    await choose(user, dialog, "Session review");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Added 1 to Session review.")).toBeInTheDocument();
    expect(calls).toEqual([{ id: SESSION_QUEUE.id, body: { items: [{ session_id: SESSION }] } }]);
  });
});
