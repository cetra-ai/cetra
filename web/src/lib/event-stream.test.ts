import { http, HttpResponse } from "msw";
import { afterEach, describe, expect, it, vi } from "vitest";
import { setUnauthorizedHandler } from "@/api/session";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { turnStream } from "@/test/turn-stream";
import { type StreamBlock, StreamRequestError, postEventStream } from "./event-stream";

const URL =
  "/api/v1/projects/acme%2Fsupport-bot/assistant/conversations/01900000-0000-7000-8000-000000001a01/turns";

afterEach(() => {
  setUnauthorizedHandler(undefined);
});

/** A `text/event-stream` answer whose body is exactly `chunks`, one enqueue per chunk. */
function chunked(chunks: string[]): Response {
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(encoder.encode(chunk));
      }
      controller.close();
    },
  });
  return new HttpResponse(body, { headers: { "Content-Type": "text/event-stream" } });
}

function serve(response: () => Response): void {
  server.use(http.post(URL, response));
}

async function collect(body: unknown = { message: "hi" }, signal = new AbortController().signal) {
  const blocks: StreamBlock[] = [];
  await postEventStream(URL, body, { signal, onBlock: (block) => blocks.push(block) });
  return blocks;
}

describe("postEventStream", () => {
  it("emits two blocks that arrive in one chunk, in order", async () => {
    serve(() =>
      chunked(['event: text\ndata: {"delta":"a"}\n\nevent: text\ndata: {"delta":"b"}\n\n']),
    );

    expect(await collect()).toEqual([
      { event: "text", data: '{"delta":"a"}' },
      { event: "text", data: '{"delta":"b"}' },
    ]);
  });

  it("joins a block split across three chunks", async () => {
    serve(() => chunked(["event: tex", 't\ndata: {"del', 'ta":"split"}\n', "\n"]));

    expect(await collect()).toEqual([{ event: "text", data: '{"delta":"split"}' }]);
  });

  it("reads CRLF line ends, including a CR split from its LF", async () => {
    serve(() =>
      chunked(["event: text\r\ndata: one\r", "\n\r\n", "event: done\r\ndata: two\r\n\r\n"]),
    );

    expect(await collect()).toEqual([
      { event: "text", data: "one" },
      { event: "done", data: "two" },
    ]);
  });

  it("joins multi-line data with newlines and ignores comments and id lines", async () => {
    serve(() => chunked([": keep-alive\n\nid: 7\ndata: first\ndata: second\n\n"]));

    expect(await collect()).toEqual([{ event: "message", data: "first\nsecond" }]);
  });

  it("skips a block that has no data line", async () => {
    serve(() => chunked(["event: ping\n\nevent: text\ndata: x\n\n"]));

    expect(await collect()).toEqual([{ event: "text", data: "x" }]);
  });

  it("keeps the last event line of a block", async () => {
    serve(() => chunked(["event: a\nevent: b\ndata: x\n\n"]));

    expect(await collect()).toEqual([{ event: "b", data: "x" }]);
  });

  it("throws StreamRequestError with the status and the parsed Problem on a 409", async () => {
    serve(() =>
      problemResponse(409, "conflict", {
        detail: "a turn is already running in this conversation",
      }),
    );

    const error = await collect().catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(StreamRequestError);
    expect((error as StreamRequestError).status).toBe(409);
    expect((error as StreamRequestError).problem).toMatchObject({ status: 409, code: "conflict" });
  });

  it("reads Retry-After as an integer of seconds from a 429", async () => {
    serve(
      () =>
        new HttpResponse(JSON.stringify({ status: 429, code: "rate_limited" }), {
          status: 429,
          headers: { "Content-Type": "application/problem+json", "Retry-After": "12" },
        }),
    );

    const error = (await collect().catch((caught: unknown) => caught)) as StreamRequestError;

    expect(error.status).toBe(429);
    expect(error.retryAfter).toBe(12);
  });

  it("calls the unauthorized handler on a 401 and then throws", async () => {
    const handler = vi.fn();
    setUnauthorizedHandler(handler);
    serve(() => problemResponse(401, "unauthorized"));

    const error = await collect().catch((caught: unknown) => caught);

    expect(handler).toHaveBeenCalledWith(URL);
    expect(error).toBeInstanceOf(StreamRequestError);
    expect((error as StreamRequestError).status).toBe(401);
  });

  it("rejects with the abort error and emits no block after the abort", async () => {
    let release: () => void = () => {};
    const hold = new Promise<void>((resolve) => {
      release = resolve;
    });
    serve(() =>
      turnStream(
        [
          { event: "text", data: { delta: "first" } },
          { event: "text", data: { delta: "late" } },
        ],
        {
          hold,
        },
      ),
    );
    const controller = new AbortController();
    const blocks: StreamBlock[] = [];

    const outcome = postEventStream(
      URL,
      { message: "hi" },
      {
        signal: controller.signal,
        onBlock: (block) => {
          blocks.push(block);
          if (blocks.length === 1) {
            controller.abort();
          }
        },
      },
    );

    await expect(outcome).rejects.toMatchObject({ name: "AbortError" });
    expect(blocks).toHaveLength(1);
    release();
  });
});
