import { HttpResponse, http } from "msw";
import { SCHEDULE_ANSWER, SCHEDULE_QUESTION } from "@/mocks/generated/story-support";
import { VOICE_AUDIO_TRACE_ID } from "@/mocks/generated/story-voice";

/**
 * A canned playground answer for the dev server's browser worker (track V, M7). A run streams
 * `start`, the text deltas and `done` with usage, cost and latency, and never reaches a provider.
 * Page tests mock their own runs with `server.use`, so this is not installed in the test server.
 */
const TRACE_ID = "0190a1b2c3d4e5f60718293a4b5c6d7e";
const STEP_MS = 150;
const encoder = new TextEncoder();

/** What a run answers: the text, streamed in three deltas, and the numbers of its `done` frame. */
type Answer = {
  /** The trace the run's "Open trace" link opens. */
  trace_id: string;
  text: string;
  input: number;
  output: number;
  cost_usd: number;
  latency_ms: number;
  ttft_ms: number;
};

/** The generated dataset's hero traces (`SHOWCASES` in `dataset.ts`): `a` and the trace index in 31 hex digits. */
const SUPPORT_TRACE_ID = "a0000000000000000000000000000003";
const PATCH_TRACE_ID = "a0000000000000000000000000000004";
const VOICE_TRACE_ID = "a0000000000000000000000000000005";
const INVOICE_TRACE_ID = "a0000000000000000000000000000006";

/**
 * The traces of the new prompt versions (P7), pinned by the test from the story datasets: the first coding run
 * tagged `context-window` (index 37), the voice turn that reads the booking back (`voiceAudioTurn`, index 110,
 * P2's rule) and the first Nordic Freight run tagged `re-extracted` (index 40).
 */
const PATCH_SPLIT_TRACE_ID = "a0000000000000000000000000000025";
const INVOICE_COMMA_TRACE_ID = "a0000000000000000000000000000028";
/** The third turn of `sess-0142` (support index 9), the schedule question: both support answers open it. */
const SUPPORT_SCHEDULE_TRACE_ID = "a0000000000000000000000000000009";

/** Any run that names no story: today's answer, and the file's existing `TRACE_ID`. */
const GENERIC: Answer = {
  trace_id: TRACE_ID,
  text: "Thanks for your message. A support agent will reply within one business day.",
  input: 42,
  output: 18,
  cost_usd: 0.0004,
  latency_ms: 900,
  ttft_ms: 300,
};

const SUPPORT: Answer = {
  trace_id: SUPPORT_TRACE_ID,
  text: "Export from the Traces page, or call the export endpoint for large ranges.\n\n- Filter the traces you need\n- Choose Export and then CSV\n- Use the API for ranges over 100,000 traces",
  input: 88,
  output: 44,
  cost_usd: 0.00066,
  latency_ms: 1150,
  ttft_ms: 360,
};

const PATCH: Answer = {
  trace_id: PATCH_TRACE_ID,
  text: "```diff\n-    return value.replace('\"', '\"\"')\n+    escaped = value.replace('\"', '\"\"')\n+    return f'\"{escaped}\"' if needs_quotes(value) else escaped\n```",
  input: 412,
  output: 58,
  cost_usd: 0.0021,
  latency_ms: 2100,
  ttft_ms: 640,
};

const VOICE: Answer = {
  trace_id: VOICE_TRACE_ID,
  text: "Sure. On Friday afternoon I have 2 pm or 4:30 pm. Which one works for you?",
  input: 74,
  output: 24,
  cost_usd: 0.00007,
  latency_ms: 540,
  ttft_ms: 190,
};

const INVOICE_FIELDS = {
  vendor: "Northwind Supplies",
  invoice_number: "INV-2026-0412",
  date: "2026-11-18",
  currency: "EUR",
  line_items: 3,
};

/** The first model's column: it reads the total as 1248, as the hero trace's first extraction did. */
const INVOICE_WRONG: Answer = {
  trace_id: INVOICE_TRACE_ID,
  text: JSON.stringify({ ...INVOICE_FIELDS, total: 1248 }, null, 2),
  input: 1480,
  output: 160,
  cost_usd: 0.00084,
  latency_ms: 1450,
  ttft_ms: 520,
};

/** The second model's column: it reads the total right, 1284, as the hero trace's retry did. */
const INVOICE_RIGHT: Answer = {
  trace_id: INVOICE_TRACE_ID,
  text: JSON.stringify({ ...INVOICE_FIELDS, total: 1284 }, null, 2),
  input: 1530,
  output: 172,
  cost_usd: 0.0072,
  latency_ms: 3200,
  ttft_ms: 880,
};

/** write-patch v4: the answer is a three-step plan, not a diff (the file has more than 20 changed files). */
const PATCH_SPLIT: Answer = {
  trace_id: PATCH_SPLIT_TRACE_ID,
  text: "This touches 23 files, so I'll do it in three steps:\n\n1. Rename `Project.owner` in the models and migrations.\n2. Update the API handlers and their tests.\n3. Update the web client and run the full suite.",
  input: 2140,
  output: 96,
  cost_usd: 0.0079,
  latency_ms: 2900,
  ttft_ms: 900,
};

/** voice-reply v5: the reply reads the booking back in one sentence. */
const VOICE_BOOKING: Answer = {
  trace_id: VOICE_AUDIO_TRACE_ID,
  text: "Done: Friday at 9:30, with Dr. Martin.",
  input: 310,
  output: 14,
  cost_usd: 0.0002,
  latency_ms: 620,
  ttft_ms: 180,
};

/** extract-invoice v6: the comma decimals read as 1234.56, whatever the model. */
const INVOICE_COMMA: Answer = {
  trace_id: INVOICE_COMMA_TRACE_ID,
  text: JSON.stringify(
    {
      vendor: "Nordic Freight",
      invoice_number: "INV-2026-0521",
      total: 1234.56,
      currency: "EUR",
    },
    null,
    2,
  ),
  input: 1180,
  output: 48,
  cost_usd: 0.0005,
  latency_ms: 1400,
  ttft_ms: 500,
};

/** support-reply v3: the answer says cetra can't schedule exports, and what to do instead. */
const SUPPORT_SCHEDULE_V3: Answer = {
  trace_id: SUPPORT_SCHEDULE_TRACE_ID,
  text: "cetra can't schedule exports yet. You can export by hand any time from the Traces page with Export → CSV, or call the export API from your own weekly job.",
  input: 1060,
  output: 52,
  cost_usd: 0.0042,
  latency_ms: 1300,
  ttft_ms: 300,
};

/** support-reply v2 on the schedule question: the wrong answer v3 corrects. */
const SUPPORT_SCHEDULE_V2: Answer = {
  trace_id: SUPPORT_SCHEDULE_TRACE_ID,
  text: SCHEDULE_ANSWER,
  input: 1040,
  output: 24,
  cost_usd: 0.0035,
  latency_ms: 1100,
  ttft_ms: 280,
};

const ANSWERS = new Map<string, Answer>([
  ["support-reply", SUPPORT],
  ["write-patch", PATCH],
  ["voice-reply", VOICE],
]);

/** The shape of the run body this mock reads; any other field is ignored. */
type RunBody = {
  model?: unknown;
  variables?: Record<string, string>;
  source?: { prompt?: { name?: unknown; version?: unknown } };
};

/**
 * The answer for a run. The prompt the draft came from (`source.prompt.name`) picks the story, and its
 * version (`source.prompt.version`, a number) picks the newer answers: support-reply v3, write-patch v4,
 * voice-reply v5 and extract-invoice v6 (whatever the model). support-reply v2 answers the schedule
 * question (`variables.question`) with the wrong answer, and any other question with today's. Older
 * versions keep their answers; for `extract-invoice` the column's model picks between the two (a model
 * whose id starts with `claude` reads the total right).
 * Anything else, including a body that is not JSON, gets GENERIC.
 */
function answerOf(body: unknown): Answer {
  const run = body as RunBody | null;
  const prompt = run?.source?.prompt;
  const name = prompt?.name;
  if (typeof name !== "string") {
    return GENERIC;
  }
  const version = prompt?.version;
  if (name === "support-reply") {
    if (version === 3) {
      return SUPPORT_SCHEDULE_V3;
    }
    if (version === 2 && run?.variables?.question === SCHEDULE_QUESTION) {
      return SUPPORT_SCHEDULE_V2;
    }
  }
  if (name === "extract-invoice") {
    if (version === 6) {
      return INVOICE_COMMA;
    }
    const model = (body as RunBody).model;
    return typeof model === "string" && model.startsWith("claude") ? INVOICE_RIGHT : INVOICE_WRONG;
  }
  if (name === "write-patch" && version === 4) {
    return PATCH_SPLIT;
  }
  if (name === "voice-reply" && version === 5) {
    return VOICE_BOOKING;
  }
  return ANSWERS.get(name) ?? GENERIC;
}

/** The text split into three consecutive deltas of whole words; their concatenation is `text`. */
function deltasOf(text: string): string[] {
  const words = text.match(/\S+\s*/g) ?? [text];
  const size = Math.ceil(words.length / 3);
  return [0, size, size * 2]
    .map((start) => words.slice(start, start + size).join(""))
    .filter((delta) => delta !== "");
}

type Frame = { wait: number; event: string; data: unknown };

/** The frames of one run; `wait` is the delay after the previous frame. */
function frames(answer: Answer): Frame[] {
  const list: Frame[] = [{ wait: 0, event: "start", data: { trace_id: answer.trace_id } }];
  for (const delta of deltasOf(answer.text)) {
    list.push({ wait: STEP_MS, event: "text", data: { delta } });
  }
  list.push({
    wait: STEP_MS,
    event: "done",
    data: {
      finish_reason: "stop",
      usage: {
        input: answer.input,
        output: answer.output,
        cache_read: 0,
        cache_write: 0,
        reasoning: 0,
      },
      cost_usd: answer.cost_usd,
      latency_ms: answer.latency_ms,
      ttft_ms: answer.ttft_ms,
      output: { role: "assistant", parts: [{ type: "text", content: answer.text }] },
    },
  });
  return list;
}

function sse(frame: Frame): Uint8Array {
  return encoder.encode(`event: ${frame.event}\ndata: ${JSON.stringify(frame.data)}\n\n`);
}

export const playgroundHandlers = [
  http.post("*/api/v1/projects/:project/playground/runs", async ({ request }) => {
    let body: unknown;
    try {
      body = await request.json();
    } catch {
      body = undefined;
    }
    const timers: ReturnType<typeof setTimeout>[] = [];
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        let at = 0;
        for (const frame of frames(answerOf(body))) {
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
    return new HttpResponse(stream, { headers: { "Content-Type": "text/event-stream" } });
  }),
];
