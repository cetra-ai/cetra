import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import queuesFixture from "../../../../fixtures/api/static/annotation-queues.json";

const TRACE = "a1000000000000000000000000000001";
const LIST = "/api/v1/projects/:project/annotation-queues";

type Queue = Schema<"AnnotationQueue">;
const QUEUE: Queue = queuesFixture.data[0] as Queue;

/**
 * Serves these queues for every list request, and returns the target ids of the requests that
 * asked which queues hold a trace or session.
 */
function serveQueues(queues: Queue[]): string[] {
  const holding: string[] = [];
  server.use(
    http.get(LIST, ({ request }) => {
      const url = new URL(request.url);
      const target =
        url.searchParams.get("target_trace_id") ?? url.searchParams.get("target_session_id");
      if (target !== null) {
        holding.push(target);
      }
      return HttpResponse.json({ data: queues, next_cursor: null });
    }),
  );
  return holding;
}

describe("queues chip", () => {
  it("renders nothing for a trace held by no queue", async () => {
    const holding = serveQueues([]);
    await renderApp(`/acme/support-bot/traces/${TRACE}`);
    await screen.findByRole("region", { name: "Trace" });
    await waitFor(() => expect(holding).toEqual([TRACE]));
    expect(screen.queryByText(/^In \d+ quer(y|ies)$/)).not.toBeInTheDocument();
  });

  it("links one queue by its id", async () => {
    serveQueues([QUEUE]);
    await renderApp(`/acme/support-bot/traces/${TRACE}`);
    const chip = await screen.findByRole("link", { name: "In 1 queue" });
    expect(chip).toHaveAttribute("href", `/acme/support-bot/annotation-queues/${QUEUE.id}`);
  });

  it("links the queue list when several queues hold the trace", async () => {
    const second: Queue = { ...QUEUE, id: "01900000-0000-7000-8000-000000000a02", name: "Two" };
    const third: Queue = { ...QUEUE, id: "01900000-0000-7000-8000-000000000a03", name: "Three" };
    serveQueues([QUEUE, second, third]);
    await renderApp(`/acme/support-bot/traces/${TRACE}`);
    const chip = await screen.findByRole("link", { name: "In 3 queues" });
    expect(chip).toHaveAttribute("href", "/acme/support-bot/annotation-queues");
  });
});
