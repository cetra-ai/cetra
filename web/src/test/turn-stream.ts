import { HttpResponse } from "msw";

/**
 * A turn's `text/event-stream` answer. The first chunk is a comment, so the headers reach the client
 * before the first event. The stream always closes normally: cancelling a live body can make undici assert.
 */
export function turnStream(
  events: { event: string; data: unknown }[],
  options: { delayMs?: number; hold?: Promise<void> } = {},
): Response {
  const encoder = new TextEncoder();
  const delayMs = options.delayMs ?? 0;
  const hold = options.hold;
  const body = new ReadableStream<Uint8Array>({
    async start(controller) {
      controller.enqueue(encoder.encode(": open\n\n"));
      for (const { event, data } of events) {
        if (delayMs > 0) {
          await new Promise((resolve) => setTimeout(resolve, delayMs));
        }
        controller.enqueue(encoder.encode(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`));
      }
      if (hold !== undefined) {
        await hold;
      }
      controller.close();
    },
  });
  return new HttpResponse(body, { headers: { "Content-Type": "text/event-stream" } });
}
