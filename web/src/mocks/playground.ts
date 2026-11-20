import { HttpResponse, http } from "msw";

/**
 * A canned playground answer for the dev server's browser worker (track V, M7). A run streams
 * `start`, three text deltas and `done` with usage, cost and latency, and never reaches a provider.
 * Page tests mock their own runs with `server.use`, so this is not installed in the test server.
 */
const TRACE_ID = "0190a1b2c3d4e5f60718293a4b5c6d7e";
const DELTAS = [
  "Thanks for your message. ",
  "A support agent will reply ",
  "within one business day.",
];
const STEP_MS = 150;
const encoder = new TextEncoder();

type Frame = { wait: number; event: string; data: unknown };

/** The frames of one run; `wait` is the delay after the previous frame. */
function frames(): Frame[] {
  const list: Frame[] = [{ wait: 0, event: "start", data: { trace_id: TRACE_ID } }];
  for (const delta of DELTAS) {
    list.push({ wait: STEP_MS, event: "text", data: { delta } });
  }
  list.push({
    wait: STEP_MS,
    event: "done",
    data: {
      finish_reason: "stop",
      usage: { input: 42, output: 18, cache_read: 0, cache_write: 0, reasoning: 0 },
      cost_usd: 0.0004,
      latency_ms: 900,
      ttft_ms: 300,
      output: { role: "assistant", parts: [{ type: "text", content: DELTAS.join("") }] },
    },
  });
  return list;
}

function sse(frame: Frame): Uint8Array {
  return encoder.encode(`event: ${frame.event}\ndata: ${JSON.stringify(frame.data)}\n\n`);
}

export const playgroundHandlers = [
  http.post("*/api/v1/projects/:project/playground/runs", () => {
    const timers: ReturnType<typeof setTimeout>[] = [];
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        let at = 0;
        for (const frame of frames()) {
          at += frame.wait;
          timers.push(
            setTimeout(() => {
              controller.enqueue(sse(frame));
            }, at),
          );
        }
        timers.push(
          setTimeout(() => {
            controller.close();
          }, at),
        );
      },
      cancel() {
        // The page aborted the run: no frame is sent after this.
        for (const timer of timers) {
          clearTimeout(timer);
        }
      },
    });
    return new HttpResponse(body, { headers: { "Content-Type": "text/event-stream" } });
  }),
];
