import { beforeEach, expect, it } from "vitest";
import { createDataset, type Dataset, isEligibleTrace } from "@/mocks/generated/dataset";
import { CODING_STORY_PROFILE } from "@/mocks/generated/story-coding";
import { EXTRACTION_STORY_PROFILE } from "@/mocks/generated/story-extraction";
import { VOICE_STORY_PROFILE } from "@/mocks/generated/story-voice";
import { server } from "@/mocks/node";
import { playgroundHandlers } from "@/mocks/playground";

const RUNS = `${location.origin}/api/v1/projects/acme%2Fsupport-bot/playground/runs`;
const NOW = Date.parse("2026-06-01T00:00:00.000Z");
const GENERIC_TRACE_ID = "0190a1b2c3d4e5f60718293a4b5c6d7e";
const GENERIC_TEXT = "Thanks for your message. A support agent will reply within one business day.";
const HOUR_MS = 3_600_000;

beforeEach(() => {
  server.use(...playgroundHandlers);
});

type Frame = { event: string; data: Record<string, unknown> };

/** Runs one request and returns its SSE frames, in order. */
async function run(body: unknown): Promise<Frame[]> {
  const response = await fetch(RUNS, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: typeof body === "string" ? body : JSON.stringify(body),
  });
  const text = await response.text();
  return text
    .split("\n\n")
    .filter((block) => block.trim() !== "")
    .map((block) => {
      const lines = block.split("\n");
      const event = lines[0]?.replace("event: ", "") ?? "";
      const data = JSON.parse(lines[1]?.replace("data: ", "") ?? "{}");
      return { event, data };
    });
}

/** The text the run streamed, joined from its text frames. */
function streamed(frames: Frame[]): string {
  return frames
    .filter((frame) => frame.event === "text")
    .map((frame) => frame.data.delta as string)
    .join("");
}

function doneOf(frames: Frame[]): Record<string, unknown> {
  const done = frames.find((frame) => frame.event === "done");
  expect(done).toBeDefined();
  return done?.data ?? {};
}

const SUPPORT_TEXT =
  "Export from the Traces page, or call the export endpoint for large ranges.\n\n- Filter the traces you need\n- Choose Export and then CSV\n- Use the API for ranges over 100,000 traces";
const PATCH_TEXT =
  "```diff\n-    return value.replace('\"', '\"\"')\n+    escaped = value.replace('\"', '\"\"')\n+    return f'\"{escaped}\"' if needs_quotes(value) else escaped\n```";
const VOICE_TEXT = "Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you?";

it("keeps today's answer for a run with no source, an unknown prompt or an unreadable body", async () => {
  const [bare, unknown, unreadable] = await Promise.all([
    run({}),
    run({ source: { prompt: { name: "summarize", version: 1 } } }),
    run("this is not json {"),
  ]);
  for (const frames of [bare, unknown, unreadable]) {
    expect(streamed(frames)).toBe(GENERIC_TEXT);
    expect(frames[0]).toEqual({ event: "start", data: { trace_id: GENERIC_TRACE_ID } });
    expect(doneOf(frames)).toMatchObject({
      usage: { input: 42, output: 18 },
      cost_usd: 0.0004,
      latency_ms: 900,
      ttft_ms: 300,
    });
  }
});

it("answers each story prompt with its text and numbers", async () => {
  const [support, patch, voice] = await Promise.all([
    run({ model: "gpt-4o", source: { prompt: { name: "support-reply", version: 2 } } }),
    run({ model: "claude-sonnet-4-5", source: { prompt: { name: "write-patch", version: 3 } } }),
    run({ model: "gpt-4.1-mini", source: { prompt: { name: "voice-reply", version: 4 } } }),
  ]);

  expect(streamed(support)).toBe(SUPPORT_TEXT);
  expect(doneOf(support)).toMatchObject({
    usage: { input: 88, output: 44 },
    cost_usd: 0.00066,
    latency_ms: 1150,
    ttft_ms: 360,
    output: { parts: [{ type: "text", content: SUPPORT_TEXT }] },
  });

  expect(streamed(patch)).toBe(PATCH_TEXT);
  expect(doneOf(patch)).toMatchObject({
    usage: { input: 412, output: 58 },
    cost_usd: 0.0021,
    latency_ms: 2100,
    ttft_ms: 640,
    output: { parts: [{ type: "text", content: PATCH_TEXT }] },
  });

  expect(streamed(voice)).toBe(VOICE_TEXT);
  expect(doneOf(voice)).toMatchObject({
    usage: { input: 74, output: 24 },
    cost_usd: 0.00007,
    latency_ms: 540,
    ttft_ms: 190,
    output: { parts: [{ type: "text", content: VOICE_TEXT }] },
  });
});

it("picks the extraction column by model", async () => {
  const source = { prompt: { name: "extract-invoice", version: 5 } };
  const [gemini, gpt, claude] = await Promise.all([
    run({ model: "gemini-2.5-flash", source }),
    run({ model: "gpt-5-mini", source }),
    run({ model: "claude-sonnet-4-5", source }),
  ]);

  expect(JSON.parse(streamed(gemini)).total).toBe(1248);
  expect(JSON.parse(streamed(gpt)).total).toBe(1248);
  expect(JSON.parse(streamed(claude)).total).toBe(1284);

  expect(doneOf(gemini)).toMatchObject({
    usage: { input: 1480, output: 160 },
    cost_usd: 0.00084,
    latency_ms: 1450,
    ttft_ms: 520,
  });
  expect(doneOf(claude)).toMatchObject({
    usage: { input: 1530, output: 172 },
    cost_usd: 0.0072,
    latency_ms: 3200,
    ttft_ms: 880,
  });
});

it("streams start, the text deltas, then done", async () => {
  const frames = await run({ model: "gpt-4o", source: { prompt: { name: "support-reply" } } });
  expect(frames[0]?.event).toBe("start");
  expect(frames[frames.length - 1]?.event).toBe("done");
  const texts = frames.filter((frame) => frame.event === "text");
  expect(texts.length).toBeGreaterThanOrEqual(1);
  expect(texts.length).toBeLessThanOrEqual(3);
  expect(frames.slice(1, -1).every((frame) => frame.event === "text")).toBe(true);

  const generic = await run({});
  expect(generic[0]).toEqual({ event: "start", data: { trace_id: GENERIC_TRACE_ID } });
});

it("opens the story's hero trace", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 10 });
  const cases: [unknown, number][] = [
    [{ source: { prompt: { name: "support-reply" } } }, 3],
    [{ source: { prompt: { name: "write-patch" } } }, 4],
    [{ source: { prompt: { name: "voice-reply" } } }, 5],
    [{ model: "gemini-2.5-flash", source: { prompt: { name: "extract-invoice" } } }, 6],
    [{ model: "claude-sonnet-4-5", source: { prompt: { name: "extract-invoice" } } }, 6],
  ];
  for (const [body, index] of cases) {
    const frames = await run(body);
    expect(frames[0]?.data.trace_id).toBe(dataset.traces[index]?.trace_id);
  }
});

it("gives both extraction columns the same fields but the total", async () => {
  const source = { prompt: { name: "extract-invoice", version: 5 } };
  const [gemini, claude] = await Promise.all([
    run({ model: "gemini-2.5-flash", source }),
    run({ model: "claude-sonnet-4-5", source }),
  ]);
  const left = JSON.parse(streamed(gemini));
  const right = JSON.parse(streamed(claude));
  const { total: leftTotal, ...leftRest } = left;
  const { total: rightTotal, ...rightRest } = right;
  expect(leftTotal).not.toBe(rightTotal);
  expect(leftRest).toEqual(rightRest);
  expect(leftRest.line_items).toBe(3);
});

const PATCH_SPLIT_TEXT =
  "This touches 23 files, so I'll do it in three steps:\n\n1. Rename `Project.owner` in the models and migrations.\n2. Update the API handlers and their tests.\n3. Update the web client and run the full suite.";
const BOOKING_TEXT = "Done: Friday at 9:30, with Dr. Martin.";

/** The voice turn that reads the booking back (P2's rule): the lowest-index availability turn of the last 2 hours that no hang-up call touches. */
function voiceBookingIndex(dataset: Dataset): number {
  const eligible = (index: number): boolean => {
    const trace = dataset.traces[index];
    return trace !== undefined && isEligibleTrace(VOICE_STORY_PROFILE, trace, index);
  };
  const calls = new Map<string, number[]>();
  dataset.traces.forEach((trace, index) => {
    const session = trace.session_id;
    if (session?.startsWith("call-") && eligible(index)) {
      calls.set(session, [...(calls.get(session) ?? []), index]);
    }
  });
  const touched = new Set<number>();
  for (const turns of calls.values()) {
    const last = turns[turns.length - 1];
    const before = turns[turns.length - 2];
    if (
      last !== undefined &&
      before !== undefined &&
      dataset.traces[last]?.tags.includes("hangup")
    ) {
      touched.add(last);
      touched.add(before);
    }
  }
  return dataset.traces.findIndex(
    (trace, index) =>
      eligible(index) &&
      !touched.has(index) &&
      trace.tags.includes("calendar") &&
      !trace.tags.includes("hangup") &&
      Date.parse(trace.start_time) >= NOW - 2 * HOUR_MS,
  );
}

it("answers the three new prompt versions with their text and numbers", async () => {
  const [patch, voice, invoice] = await Promise.all([
    run({ model: "claude-sonnet-4-5", source: { prompt: { name: "write-patch", version: 4 } } }),
    run({ model: "gpt-4.1-mini", source: { prompt: { name: "voice-reply", version: 5 } } }),
    run({ model: "gemini-2.5-flash", source: { prompt: { name: "extract-invoice", version: 6 } } }),
  ]);

  expect(streamed(patch)).toBe(PATCH_SPLIT_TEXT);
  expect(doneOf(patch)).toMatchObject({
    usage: { input: 2140, output: 96 },
    cost_usd: 0.0079,
    latency_ms: 2900,
  });

  expect(streamed(voice)).toBe(BOOKING_TEXT);
  expect(doneOf(voice)).toMatchObject({
    usage: { input: 310, output: 14 },
    cost_usd: 0.0002,
    latency_ms: 620,
    ttft_ms: 180,
  });

  expect(JSON.parse(streamed(invoice))).toEqual({
    vendor: "Nordic Freight",
    invoice_number: "INV-2026-0521",
    total: 1234.56,
    currency: "EUR",
  });
  expect(doneOf(invoice)).toMatchObject({
    usage: { input: 1180, output: 48 },
    cost_usd: 0.0005,
    latency_ms: 1400,
  });
});

it("keeps the older versions' answers, and answers extract-invoice v6 the same for every model", async () => {
  const [oldPatch, oldVoice, gemini, claude] = await Promise.all([
    run({ model: "claude-sonnet-4-5", source: { prompt: { name: "write-patch", version: 3 } } }),
    run({ model: "gpt-4.1-mini", source: { prompt: { name: "voice-reply", version: 4 } } }),
    run({ model: "gemini-2.5-flash", source: { prompt: { name: "extract-invoice", version: 6 } } }),
    run({
      model: "claude-sonnet-4-5",
      source: { prompt: { name: "extract-invoice", version: 6 } },
    }),
  ]);
  expect(streamed(oldPatch)).toBe(PATCH_TEXT);
  expect(streamed(oldVoice)).toBe(VOICE_TEXT);
  expect(streamed(claude)).toBe(streamed(gemini));
});

it("opens the trace of each new version: the first context-window run, the booking turn, the first Nordic Freight re-extraction", async () => {
  const coding = createDataset({ seed: 1, now: NOW, traces: 2000, profile: CODING_STORY_PROFILE });
  const codingIndex = coding.traces.findIndex((trace) => trace.tags.includes("context-window"));
  expect(codingIndex).toBe(37);
  const patch = await run({ source: { prompt: { name: "write-patch", version: 4 } } });
  expect(patch[0]?.data.trace_id).toBe(coding.traces[codingIndex]?.trace_id);
  expect(patch[0]?.data.trace_id).toBe("a0000000000000000000000000000025");

  const voice = createDataset({ seed: 1, now: NOW, traces: 2000, profile: VOICE_STORY_PROFILE });
  const bookingIndex = voiceBookingIndex(voice);
  expect(bookingIndex).toBe(110);
  const bookingTrace = voice.traces[bookingIndex]?.trace_id;
  expect(voice.traceDetail(bookingTrace ?? "")?.spans.at(-1)?.span.name).toBe("check_availability");
  const booking = await run({ source: { prompt: { name: "voice-reply", version: 5 } } });
  expect(booking[0]?.data.trace_id).toBe(bookingTrace);
  expect(booking[0]?.data.trace_id).toBe("a000000000000000000000000000006e");

  const extraction = createDataset({
    seed: 1,
    now: NOW,
    traces: 2000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  const nordic = "invoice-2026-0521.pdf (2 pages)";
  const nordicIndex = extraction.traces.findIndex(
    (trace, index) =>
      isEligibleTrace(EXTRACTION_STORY_PROFILE, trace, index) &&
      trace.input_preview === nordic &&
      trace.tags.includes("re-extracted"),
  );
  expect(nordicIndex).toBe(40);
  const invoice = await run({ source: { prompt: { name: "extract-invoice", version: 6 } } });
  expect(invoice[0]?.data.trace_id).toBe(extraction.traces[nordicIndex]?.trace_id);
  expect(invoice[0]?.data.trace_id).toBe("a0000000000000000000000000000028");
});
