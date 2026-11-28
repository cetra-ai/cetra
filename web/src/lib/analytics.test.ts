import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import {
  type AnalyticsContext,
  postEvents,
  startAnalytics,
  track,
  type WireEvent,
} from "@/lib/analytics";
import { server } from "@/mocks/node";

const stops: Array<() => void> = [];

afterEach(() => {
  for (const stop of stops.splice(0)) {
    stop();
  }
  Reflect.deleteProperty(document, "visibilityState");
});

function start(options: Parameters<typeof startAnalytics>[0]): () => void {
  const stop = startAnalytics(options);
  stops.push(stop);
  return stop;
}

it("track does nothing before start", async () => {
  const send = vi.fn(async () => {});
  track({ name: "command_palette.opened" });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(send).not.toHaveBeenCalled();
});

it("sends queued events after the interval", async () => {
  const send = vi.fn(async (_events: WireEvent[]) => {});
  start({ send, flushMs: 10 });
  track({ name: "command_palette.opened" });
  track({ name: "assistant.panel_opened" });
  await vi.waitFor(() => expect(send).toHaveBeenCalledTimes(1));
  expect(send.mock.calls[0]?.[0]).toEqual([
    { name: "command_palette.opened" },
    { name: "assistant.panel_opened" },
  ]);
});

it("drops a batch without a window and never throws from a late timer", async () => {
  const send = vi.fn(async (_events: WireEvent[]) => {});
  const stop = start({ send, flushMs: 5 });
  track({ name: "command_palette.opened" });
  vi.stubGlobal("window", undefined);
  try {
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(() => stop()).not.toThrow();
  } finally {
    vi.unstubAllGlobals();
  }
  expect(send).not.toHaveBeenCalled();
});

it("adds the context's org and project", async () => {
  const send = vi.fn(async (_events: WireEvent[]) => {});
  const context: AnalyticsContext = { org: "acme", project: "support-bot" };
  const stop = start({ send, flushMs: 60_000, context: () => context });
  track({ name: "command_palette.opened" });
  stop();
  expect(send.mock.calls[0]?.[0]).toEqual([
    { name: "command_palette.opened", org: "acme", project: "support-bot" },
  ]);

  const empty = vi.fn(async (_events: WireEvent[]) => {});
  const stopEmpty = start({ send: empty, flushMs: 60_000 });
  track({ name: "command_palette.opened" });
  stopEmpty();
  expect(empty.mock.calls[0]?.[0]).toEqual([{ name: "command_palette.opened" }]);
  expect(Object.keys(empty.mock.calls[0]?.[0][0] ?? {})).toEqual(["name"]);
});

it("sends at most 50 events per batch", async () => {
  const send = vi.fn(async (_events: WireEvent[]) => {});
  const stop = start({ send, flushMs: 60_000 });
  for (let index = 0; index < 120; index += 1) {
    track({ name: "live_tail.toggled", properties: { on: index % 2 === 0 } });
  }
  expect(send).toHaveBeenCalledTimes(2);
  expect(send.mock.calls[0]?.[0]).toHaveLength(50);
  expect(send.mock.calls[1]?.[0]).toHaveLength(50);
  stop();
  expect(send).toHaveBeenCalledTimes(3);
  expect(send.mock.calls[2]?.[0]).toHaveLength(20);
  const sent = send.mock.calls.flatMap((call) => call[0]);
  expect(sent).toHaveLength(120);
  expect(sent.map((event) => event.properties)).toEqual(
    Array.from({ length: 120 }, (_, index) => ({ on: index % 2 === 0 })),
  );
});

it("a replaced start's stop does nothing", async () => {
  const a = vi.fn(async (_events: WireEvent[]) => {});
  const b = vi.fn(async (_events: WireEvent[]) => {});
  const stopA = startAnalytics({ send: a, flushMs: 60_000 });
  const stopB = startAnalytics({ send: b, flushMs: 60_000 });
  stopA();
  track({ name: "command_palette.opened" });
  stopB();
  expect(b).toHaveBeenCalledTimes(1);
  expect(b.mock.calls[0]?.[0]).toEqual([{ name: "command_palette.opened" }]);
  expect(a).not.toHaveBeenCalled();
});

it("a hidden page flushes", async () => {
  const send = vi.fn(async (_events: WireEvent[]) => {});
  start({ send, flushMs: 60_000 });
  track({ name: "command_palette.opened" });
  Object.defineProperty(document, "visibilityState", { configurable: true, value: "hidden" });
  document.dispatchEvent(new Event("visibilitychange"));
  expect(send).toHaveBeenCalledTimes(1);
  expect(send.mock.calls[0]?.[0]).toEqual([{ name: "command_palette.opened" }]);
});

it("stop stops tracking", async () => {
  const send = vi.fn(async (_events: WireEvent[]) => {});
  const stop = start({ send, flushMs: 60_000 });
  stop();
  track({ name: "command_palette.opened" });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(send).not.toHaveBeenCalled();
});

it("a failed send is ignored", async () => {
  let calls = 0;
  const send = vi.fn(async (_events: WireEvent[]) => {
    calls += 1;
    if (calls === 1) {
      throw new Error("offline");
    }
  });
  start({ send, flushMs: 10 });
  track({ name: "command_palette.opened" });
  await vi.waitFor(() => expect(send).toHaveBeenCalledTimes(1));
  track({ name: "assistant.panel_opened" });
  await vi.waitFor(() => expect(send).toHaveBeenCalledTimes(2));
  expect(send.mock.calls[1]?.[0]).toEqual([{ name: "assistant.panel_opened" }]);
});

it("postEvents posts JSON to the events endpoint", async () => {
  let captured: { type: string | null; body: unknown } | undefined;
  server.use(
    http.post("/api/v1/analytics/events", async ({ request }) => {
      captured = { type: request.headers.get("content-type"), body: await request.json() };
      return new HttpResponse(null, { status: 204 });
    }),
  );
  await postEvents([{ name: "command_palette.opened" }]);
  expect(captured?.type?.startsWith("application/json")).toBe(true);
  expect(captured?.body).toEqual({ events: [{ name: "command_palette.opened" }] });
});
