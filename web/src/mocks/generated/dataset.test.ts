import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset, type Dataset, SHOWCASES, SUPPORT_TURNS } from "@/mocks/generated/dataset";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");

/** The topic whose main question is `question`, if any. */
function topicOf(question: string | undefined): (typeof SUPPORT_TURNS)[number] | undefined {
  return SUPPORT_TURNS.find((topic) => topic.question === question);
}

/** The question turn `turn` of a session asks: turn 1 the main question, turn k the follow-up `(k - 2) mod count`. */
function expectedQuestion(
  topic: { question: string; followUps: readonly { question: string }[] },
  turn: number,
): string | undefined {
  if (turn === 1) {
    return topic.question;
  }
  return topic.followUps[(turn - 2) % topic.followUps.length]?.question;
}

/** The question a span's input carries: a text input, a user message, or a reranker's `query`. */
function questionOf(detail: Schema<"SpanDetail"> | undefined): string | undefined {
  const input = detail?.input;
  if (!input) {
    return undefined;
  }
  if (input.format === "text") {
    return typeof input.value === "string" ? input.value : undefined;
  }
  if (input.format === "messages") {
    const part = input.value.find((message) => message.role === "user")?.parts[0];
    return part?.type === "text" && typeof part.content === "string" ? part.content : undefined;
  }
  const value: unknown = input.value;
  if (typeof value === "object" && value !== null && "query" in value) {
    return typeof value.query === "string" ? value.query : undefined;
  }
  return undefined;
}

function rootQuestion(dataset: Dataset, trace: Schema<"TraceSummary">): string | undefined {
  if (!trace.root_span_id) {
    return undefined;
  }
  return questionOf(dataset.spanDetail(trace.trace_id, trace.root_span_id));
}

it("gives every span of a trace the trace's topic", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 4 });
  let llmChecked = 0;
  // Trace 0 holds 10,000 spans, so the first traces that matter here are 1 to 3.
  for (const trace of dataset.traces.slice(1)) {
    const topic = rootQuestion(dataset, trace);
    expect(topic).toBeDefined();
    expect(trace.input_preview).toBe(topic);

    for (const row of dataset.traceDetail(trace.trace_id)?.spans ?? []) {
      const detail = dataset.spanDetail(trace.trace_id, row.span.span_id);
      if (row.span.span_kind === "llm") {
        llmChecked += 1;
      }
      const question = questionOf(detail);
      if (question !== undefined) {
        expect(question).toBe(topic);
      }
    }
  }
  expect(llmChecked).toBeGreaterThan(0);
});

it("picks a different topic for a different trace", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 3 });
  const [, first, second] = dataset.traces;
  expect(first).toBeDefined();
  expect(second).toBeDefined();
  if (!first || !second) {
    throw new Error("expected traces 1 and 2");
  }
  const firstTopic = rootQuestion(dataset, first);
  const secondTopic = rootQuestion(dataset, second);
  expect(firstTopic).toBeDefined();
  expect(secondTopic).toBeDefined();
  expect(firstTopic).not.toBe(secondTopic);
});

it("gives every trace of a session the session's topic, turn by turn in start-time order", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  // Trace 0 is 10,000 spans long, so sessions that contain it are left out.
  const traceZero = dataset.traces[0]?.trace_id;
  const groups = new Map<string, Schema<"TraceSummary">[]>();
  for (const trace of dataset.traces) {
    if (trace.session_id) {
      groups.set(trace.session_id, [...(groups.get(trace.session_id) ?? []), trace]);
    }
  }
  const sessions = [...groups.values()]
    .filter((rows) => rows.length >= 3 && rows.every((row) => row.trace_id !== traceZero))
    .map((rows) => [...rows].sort((left, right) => left.start_time.localeCompare(right.start_time)))
    .slice(0, 5);
  expect(sessions.length).toBeGreaterThanOrEqual(2);

  for (const rows of sessions) {
    const [first] = rows;
    if (!first) {
      throw new Error("a session has at least three traces");
    }
    const topic = topicOf(rootQuestion(dataset, first));
    expect(topic).toBeDefined();
    if (!topic) {
      throw new Error("turn 1 must ask a topic's main question");
    }
    rows.forEach((trace, position) => {
      const turn = position + 1;
      const expected = expectedQuestion(topic, turn);
      expect(expected).toBeDefined();
      expect(trace.input_preview).toBe(expected);
      expect(rootQuestion(dataset, trace)).toBe(expected);

      for (const row of dataset.traceDetail(trace.trace_id)?.spans ?? []) {
        const detail = dataset.spanDetail(trace.trace_id, row.span.span_id);
        const question = questionOf(detail);
        if (question !== undefined) {
          expect(question).toBe(expected);
        }
        if (row.span.span_kind === "llm" && turn > 1) {
          // Only the main answer carries a code block; a follow-up is a paragraph and a list.
          const reply =
            detail?.output?.format === "messages" ? detail.output.value[0]?.parts[0] : undefined;
          const text = reply?.type === "text" ? reply.content : "";
          expect(text).toMatch(/^- /m);
          expect(text).not.toContain("```");
        }
      }
    });
  }
});

it("starts a session's turns 30 s to 8 min apart, and no session lasts 7 hours", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  const groups = new Map<string, Schema<"TraceSummary">[]>();
  for (const trace of dataset.traces) {
    if (trace.session_id) {
      groups.set(trace.session_id, [...(groups.get(trace.session_id) ?? []), trace]);
    }
  }
  let gaps = 0;
  for (const rows of groups.values()) {
    const ordered = [...rows].sort((left, right) =>
      left.start_time.localeCompare(right.start_time),
    );
    for (let turn = 1; turn < ordered.length; turn += 1) {
      const previous = ordered[turn - 1];
      const current = ordered[turn];
      if (!previous || !current) {
        throw new Error("turns are consecutive");
      }
      const gap = Date.parse(current.start_time) - Date.parse(previous.start_time);
      expect(gap).toBeGreaterThanOrEqual(30_000);
      expect(gap).toBeLessThanOrEqual(480_000);
      gaps += 1;
    }
    const first = ordered[0];
    const last = ordered[ordered.length - 1];
    if (!first || !last) {
      throw new Error("a session has traces");
    }
    expect(Date.parse(last.end_time) - Date.parse(first.start_time)).toBeLessThan(7 * 3_600_000);
    expect(Date.parse(last.start_time)).toBeLessThanOrEqual(NOW);
  }
  expect(gaps).toBeGreaterThan(0);
});

it("gives a trace without a session a topic of its own", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 40 });
  const sessionless = dataset.traces
    .slice(1)
    .filter(
      (trace, i) =>
        !trace.session_id &&
        !SHOWCASES.some((showcase) => showcase.index === i + 1 && showcase.story),
    );
  expect(sessionless.length).toBeGreaterThan(0);
  for (const trace of sessionless.slice(0, 5)) {
    expect(topicOf(trace.input_preview)).toBeDefined();
    expect(rootQuestion(dataset, trace)).toBe(trace.input_preview);
  }
});

it("gives an llm span a messages input and a Markdown output with a fenced code block", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 20 });
  let found: { traceId: string; spanId: string } | undefined;
  for (const trace of dataset.traces) {
    const row = dataset
      .traceDetail(trace.trace_id)
      ?.spans.find((item) => item.span.span_kind === "llm");
    if (row) {
      found = { traceId: trace.trace_id, spanId: row.span.span_id };
      break;
    }
  }
  expect(found).toBeDefined();
  const detail = dataset.spanDetail(found?.traceId ?? "", found?.spanId ?? "");

  expect(detail?.input?.format).toBe("messages");
  expect(detail?.output?.format).toBe("messages");
  if (detail?.input?.format !== "messages" || detail.output?.format !== "messages") {
    throw new Error("llm span payloads must be messages");
  }
  expect(detail.input.value.map((message) => message.role)).toEqual(["system", "user"]);
  const answer = detail.output.value[0]?.parts[0];
  expect(answer?.type).toBe("text");
  const text = answer?.type === "text" ? answer.content : "";
  expect(text).toMatch(/^## /m);
  expect(text).toMatch(/^- /m);
  expect(text).toMatch(/^```(python|ts)$/m);
});

it("gives each session the previews of its earliest and latest traces and counts its error traces", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  const groups = new Map<string, Schema<"TraceSummary">[]>();
  for (const trace of dataset.traces) {
    if (trace.session_id) {
      groups.set(trace.session_id, [...(groups.get(trace.session_id) ?? []), trace]);
    }
  }
  const sessions = dataset.sessions(true, true);
  expect(sessions.length).toBeGreaterThan(0);

  let withErrors = 0;
  let multiTurn = 0;
  for (const session of sessions) {
    const rows = [...(groups.get(session.session_id) ?? [])].sort(
      (left, right) =>
        left.start_time.localeCompare(right.start_time) ||
        left.trace_id.localeCompare(right.trace_id),
    );
    const first = rows[0];
    const last = rows[rows.length - 1];
    if (!first || !last) {
      throw new Error("a listed session has traces");
    }
    expect(session.first_input_preview).toBe(first.input_preview);
    expect(session.last_output_preview).toBe(last.output_preview);
    expect(session.error_trace_count).toBe(rows.filter((row) => row.status === "error").length);
    if (session.error_trace_count > 0) {
      withErrors += 1;
    }
    if (rows.length > 1) {
      multiTurn += 1;
      // The first turn is the topic's main question and the last one a follow-up, so they differ.
      expect(session.first_input_preview).not.toBe(last.input_preview);
    }
  }
  expect(withErrors).toBeGreaterThan(0);
  expect(multiTurn).toBeGreaterThan(0);
});

const DAY_MS = 86_400_000;

/** A trace the default lists show: no evaluation and no experiment trace. */
function isVisible(trace: Schema<"TraceSummary">): boolean {
  return trace.environment !== "cetra-eval" && trace.metadata["cetra.experiment_id"] === undefined;
}

/** The visible trace of the user with the most visible traces, so that its traces span several days. */
function busiestUser(dataset: Dataset): string {
  const counts = new Map<string, number>();
  for (const trace of dataset.traces) {
    if (trace.user_id && isVisible(trace)) {
      counts.set(trace.user_id, (counts.get(trace.user_id) ?? 0) + 1);
    }
  }
  const [userId] = [...counts.entries()].sort((left, right) => right[1] - left[1])[0] ?? [];
  if (!userId) {
    throw new Error("the dataset has a user with visible traces");
  }
  return userId;
}

it("counts a user's traces in a window only: trace count, first seen, sessions and activity", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  const userId = busiestUser(dataset);
  const mine = dataset.traces.filter((trace) => trace.user_id === userId);
  // The UTC day of the user's latest trace: some of the user's traces are on other days.
  const latest = Math.max(...mine.map((trace) => Date.parse(trace.start_time)));
  const day = Math.floor(latest / DAY_MS) * DAY_MS;
  const inDay = mine.filter((trace) => {
    const start = Date.parse(trace.start_time);
    return start >= day && start < day + DAY_MS;
  });
  expect(inDay.length).toBeGreaterThan(0);
  expect(inDay.length).toBeLessThan(mine.length);

  const detail = dataset.user(userId, false, false, { from: day, to: day + DAY_MS });
  expect(detail?.user.trace_count).toBe(inDay.length);
  expect(Date.parse(detail?.user.first_seen ?? "")).toBe(
    Math.min(...inDay.map((trace) => Date.parse(trace.start_time))),
  );
  expect(detail?.user.activity.reduce((sum, count) => sum + count, 0)).toBe(inDay.length);

  const sessionIds = new Set(
    inDay.flatMap((trace) => (trace.session_id ? [trace.session_id] : [])),
  );
  expect(detail?.sessions.map((session) => session.session_id).sort()).toEqual(
    [...sessionIds].sort(),
  );
  for (const session of detail?.sessions ?? []) {
    expect(session.trace_count).toBe(
      inDay.filter((trace) => trace.session_id === session.session_id).length,
    );
  }
});

it("has no user detail for a window without any of the user's traces", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  const userId = busiestUser(dataset);
  const from = Date.parse("2000-01-01T00:00:00.000Z");
  expect(dataset.user(userId, false, false, { from, to: from + DAY_MS })).toBeUndefined();
});

/** The showcase trace (index 3) of a ten-trace dataset, with its tree. */
function showcaseOf(): {
  dataset: Dataset;
  trace: Schema<"TraceSummary">;
  spans: readonly Schema<"TreeSpan">[];
} {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 10 });
  const trace = dataset.traces[3];
  if (!trace) {
    throw new Error("a ten-trace dataset has trace 3");
  }
  return { dataset, trace, spans: dataset.traceDetail(trace.trace_id)?.spans ?? [] };
}

/** The detail of the showcase span named `name`. */
function showcaseSpanNamed(name: string): Schema<"SpanDetail"> {
  const { dataset, trace, spans } = showcaseOf();
  const row = spans.find((item) => item.span.name === name);
  const detail = row ? dataset.spanDetail(trace.trace_id, row.span.span_id) : undefined;
  if (!detail) {
    throw new Error(`the showcase trace has a span named ${name}`);
  }
  return detail;
}

it("makes trace 3 the hand-written showcase trace", () => {
  const { trace, spans } = showcaseOf();
  const nonRoot = spans.slice(1);
  expect(trace.name).toBe("support-agent");
  expect(trace.session_id).toBeUndefined();
  expect(spans.length).toBe(13);
  expect(trace.span_count).toBe(spans.length);
  expect(trace.llm_count).toBe(3);
  expect(trace.llm_count).toBe(spans.filter((row) => row.span.span_kind === "llm").length);

  const inputSum = nonRoot.reduce((sum, row) => sum + (row.span.input_tokens ?? 0), 0);
  const outputSum = nonRoot.reduce((sum, row) => sum + (row.span.output_tokens ?? 0), 0);
  const costSum = nonRoot.reduce((sum, row) => sum + (row.span.cost_total ?? 0), 0);
  expect(trace.input_tokens).toBe(inputSum);
  expect(trace.output_tokens).toBe(outputSum);
  expect(trace.cost_total).toBe(Math.round(costSum * 1_000_000) / 1_000_000);

  expect(trace.input_preview).toBe("How can I export my traces to a CSV file?");
  expect(Date.parse(trace.start_time)).toBe(NOW - 120_000);
});

it("nests every showcase span inside its parent, in pre-order", () => {
  const { spans } = showcaseOf();
  let children = 0;
  spans.forEach((row, position) => {
    const parentId = row.span.parent_span_id;
    if (parentId === undefined) {
      expect(row.depth).toBe(0);
      return;
    }
    children += 1;
    const parentPosition = spans.findIndex((item) => item.span.span_id === parentId);
    expect(parentPosition).toBeGreaterThanOrEqual(0);
    expect(parentPosition).toBeLessThan(position);
    const parent = spans[parentPosition];
    if (!parent) {
      throw new Error("the parent is in the list");
    }
    expect(row.depth).toBe(parent.depth + 1);
    expect(Date.parse(row.span.start_time)).toBeGreaterThanOrEqual(
      Date.parse(parent.span.start_time),
    );
    expect(Date.parse(row.span.end_time)).toBeLessThanOrEqual(Date.parse(parent.span.end_time));
  });
  expect(children).toBe(12);
});

it("runs the two showcase tools in parallel", () => {
  const { spans } = showcaseOf();
  const accountTool = spans.find((row) => row.span.name === "get_account");
  const exportTool = spans.find((row) => row.span.name === "check_export_limits");
  expect(accountTool).toBeDefined();
  expect(exportTool).toBeDefined();
  expect(accountTool?.span.start_time).toBe(exportTool?.span.start_time);
});

it("gives the showcase spans their payloads", () => {
  const classify = showcaseSpanNamed("classify intent");
  expect(classify.output?.format).toBe("messages");
  const classifyText =
    classify.output?.format === "messages" ? classify.output.value[0]?.parts[0] : undefined;
  expect(classifyText?.type === "text" ? classifyText.content : "").toContain("export_data");

  const exportLimits = showcaseSpanNamed("check_export_limits");
  expect(exportLimits.output?.format).toBe("json");
  expect(exportLimits.output?.value).toMatchObject({ max_rows: 100_000 });

  const judge = showcaseSpanNamed("judge helpfulness");
  expect(judge.output?.value).toMatchObject({ score: 0.9 });

  const draft = showcaseSpanNamed("draft answer");
  const draftText =
    draft.output?.format === "messages" ? draft.output.value[0]?.parts[0] : undefined;
  expect(draftText?.type === "text" ? draftText.content : "").toContain("```");
});

it("gives no showcase span a truncation flag", () => {
  const { dataset, trace, spans } = showcaseOf();
  expect(spans.length).toBeGreaterThan(0);
  for (const row of spans) {
    const detail = dataset.spanDetail(trace.trace_id, row.span.span_id);
    expect(detail?.truncated).toEqual([]);
  }
});

/** The scenario showcase `index` of a ten-trace dataset, with its trace and tree. */
function scenarioOf(index: number): {
  dataset: Dataset;
  trace: Schema<"TraceSummary">;
  spans: readonly Schema<"TreeSpan">[];
} {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 10 });
  const trace = dataset.traces[index];
  if (!trace) {
    throw new Error(`a ten-trace dataset has trace ${index}`);
  }
  const detail = dataset.traceDetail(trace.trace_id);
  if (!detail) {
    throw new Error(`trace ${index} has a detail`);
  }
  return { dataset, trace, spans: detail.spans };
}

it("makes traces 4 to 6 the hand-written scenario traces", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 10 });
  for (const index of [4, 5, 6]) {
    const showcase = SHOWCASES.find((s) => s.index === index);
    const story = showcase?.story;
    const trace = dataset.traces[index];
    if (!showcase || !story || !trace) {
      throw new Error(`trace ${index} and its showcase with a story exist`);
    }
    const spans = dataset.traceDetail(trace.trace_id)?.spans ?? [];
    const nonRoot = spans.slice(1);
    expect(trace.name).toBe(showcase.spans[0].name);
    expect(trace.session_id).toBeUndefined();
    expect(trace.span_count).toBe(spans.length);
    expect(trace.llm_count).toBe(spans.filter((row) => row.span.span_kind === "llm").length);

    const inputSum = nonRoot.reduce((sum, row) => sum + (row.span.input_tokens ?? 0), 0);
    const outputSum = nonRoot.reduce((sum, row) => sum + (row.span.output_tokens ?? 0), 0);
    const costSum = nonRoot.reduce((sum, row) => sum + (row.span.cost_total ?? 0), 0);
    expect(trace.input_tokens).toBe(inputSum);
    expect(trace.output_tokens).toBe(outputSum);
    expect(trace.cost_total).toBe(Math.round(costSum * 1_000_000) / 1_000_000);

    expect(trace.input_preview).toBe(story.inputPreview);
    expect(trace.output_preview).toBe(story.outputPreview);
    expect(Date.parse(trace.start_time)).toBe(NOW - showcase.ageMs);
    expect(trace.duration_ns).toBe(showcase.spans[0].to * 1_000_000);
  }
});

it("nests every span of every showcase inside its parent, in pre-order", () => {
  for (const showcase of SHOWCASES) {
    const { spans } = scenarioOf(showcase.index);
    expect(spans.length).toBe(showcase.spans.length);
    spans.forEach((row, position) => {
      const parentId = row.span.parent_span_id;
      const expected = showcase.spans[position]?.parent;
      expect(parentId).toBe(expected === undefined ? undefined : spans[expected]?.span.span_id);
      if (parentId === undefined) {
        expect(row.depth).toBe(0);
        return;
      }
      const parentPosition = spans.findIndex((item) => item.span.span_id === parentId);
      expect(parentPosition).toBeGreaterThanOrEqual(0);
      expect(parentPosition).toBeLessThan(position);
      const parent = spans[parentPosition];
      if (!parent) {
        throw new Error("the parent is in the list");
      }
      expect(row.depth).toBe(parent.depth + 1);
      expect(Date.parse(row.span.start_time)).toBeGreaterThanOrEqual(
        Date.parse(parent.span.start_time),
      );
      expect(Date.parse(row.span.end_time)).toBeLessThanOrEqual(Date.parse(parent.span.end_time));
    });
  }
});

it("gives every span of a scenario its hand-written payloads", () => {
  for (const showcase of SHOWCASES) {
    const story = showcase.story;
    if (!story) {
      continue;
    }
    const { dataset, trace, spans } = scenarioOf(showcase.index);
    expect(story.payloads.length).toBe(spans.length);
    spans.forEach((row, position) => {
      const payload = story.payloads[position];
      const detail = dataset.spanDetail(trace.trace_id, row.span.span_id);
      expect(detail?.input).toEqual(payload?.input);
      expect(detail?.output).toEqual(payload?.output);
      expect(detail?.truncated).toEqual([]);
    });
  }
});

it("fails the coding agent's first test run", () => {
  const { dataset, trace, spans } = scenarioOf(4);
  expect(trace.status).toBe("error");
  expect(trace.error_count).toBe(1);

  const failed = spans[8];
  if (!failed) {
    throw new Error("the coding agent has a span at position 8");
  }
  expect(failed.span.name).toBe("run_tests");
  expect(failed.span.status).toBe("error");
  expect(failed.span.status_message).toBe("1 failed, 47 passed");
  const failedDetail = dataset.spanDetail(trace.trace_id, failed.span.span_id);
  expect(failedDetail?.events.length).toBe(1);
  const event = failedDetail?.events[0];
  if (!event) {
    throw new Error("the failed test run has one event");
  }
  expect(event.name).toBe("exception");
  expect(event.attributes).toMatchObject({ "exception.message": "1 failed, 47 passed" });

  const passed = spans[12];
  if (!passed) {
    throw new Error("the coding agent has a span at position 12");
  }
  expect(passed.span.name).toBe("run_tests");
  expect(passed.span.status).toBe("ok");
  const passedDetail = dataset.spanDetail(trace.trace_id, passed.span.span_id);
  expect(passedDetail?.events).toEqual([]);
});

it("makes the voice agent's availability check its slowest span", () => {
  const { trace, spans } = scenarioOf(5);
  expect(trace.name).toBe("voice-agent");
  expect(trace.duration_ns).toBe(3_100_000_000);
  const slowest = spans
    .slice(1)
    .reduce((best, row) => (row.span.duration_ns > best.span.duration_ns ? row : best));
  expect(slowest.span.name).toBe("check_availability");
  expect(spans.indexOf(slowest)).toBe(4);
  expect(slowest.span.span_id).toBe("0000000500000005");
});

it("scores the voice agent's reply over its latency budget", () => {
  const { trace } = scenarioOf(5);
  expect(trace.scores.map((score) => score.name)).toEqual(["under_budget"]);
  expect(trace.scores[0]?.value).toEqual({ data_type: "boolean", value: false });
});

it("fails the invoice's first schema check and passes the second", () => {
  const { dataset, trace, spans } = scenarioOf(6);
  const first = spans[4];
  if (!first) {
    throw new Error("the invoice trace has a span at position 4");
  }
  const firstDetail = dataset.spanDetail(trace.trace_id, first.span.span_id);
  expect(firstDetail?.output?.value).toMatchObject({ passed: false });

  const second = spans[6];
  if (!second) {
    throw new Error("the invoice trace has a span at position 6");
  }
  const secondDetail = dataset.spanDetail(trace.trace_id, second.span.span_id);
  expect(secondDetail?.output?.value).toMatchObject({ passed: true });
});

/** A generated trace is one that is neither trace 0 nor a showcase (M14). */
function generatedIndices(traces: number): number[] {
  const showcases = new Set(SHOWCASES.map((showcase) => showcase.index));
  const indices: number[] = [];
  for (let index = 1; index < traces; index += 1) {
    if (!showcases.has(index)) {
      indices.push(index);
    }
  }
  return indices;
}

it("gives every generated voice trace a duration between 0.4 and 4 s", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  const voice = generatedIndices(2000)
    .map((index) => dataset.traces[index])
    .filter((trace) => trace?.tags.includes("voice"));
  expect(voice.length).toBeGreaterThan(100);
  for (const trace of voice) {
    const ms = (trace?.duration_ns ?? 0) / 1_000_000;
    expect(Number.isInteger(ms)).toBe(true);
    expect(ms).toBeGreaterThanOrEqual(400);
    expect(ms).toBeLessThanOrEqual(3_999);
  }
});

it("keeps the drawn duration range of the other generated traces", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  const others = generatedIndices(2000)
    .map((index) => dataset.traces[index])
    .filter((trace) => trace && !trace.tags.includes("voice"));
  for (const trace of others) {
    const ms = (trace?.duration_ns ?? 0) / 1_000_000;
    expect(ms).toBeGreaterThanOrEqual(200);
    expect(ms).toBeLessThanOrEqual(20_199);
  }
  expect(others.some((trace) => (trace?.duration_ns ?? 0) / 1_000_000 > 4_000)).toBe(true);
});

it("draws the same durations as before for the traces it does not rescale", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 60 });
  // Non-voice generated traces: [index, duration in ms] as drawn before M14.
  const unchanged: [number, number][] = [
    [1, 13179],
    [2, 12086],
    [7, 809],
    [8, 16499],
    [9, 3064],
    [10, 19521],
    [11, 16920],
    [12, 18981],
    [13, 17386],
    [14, 17650],
    [15, 12310],
    [17, 14573],
  ];
  for (const [index, ms] of unchanged) {
    expect(dataset.traces[index]?.tags.includes("voice")).toBe(false);
    expect(dataset.traces[index]?.duration_ns).toBe(ms * 1_000_000);
  }
  // Voice-tagged generated traces: [index, duration in ms as drawn before M14], rescaled to 400 + (ms - 200) * 3600 / 20000.
  const drawn: [number, number][] = [
    [16, 5934],
    [19, 14403],
    [28, 4041],
    [35, 905],
  ];
  for (const [index, ms] of drawn) {
    expect(dataset.traces[index]?.tags.includes("voice")).toBe(true);
    expect(dataset.traces[index]?.duration_ns).toBe(
      (400 + Math.floor(((ms - 200) * 3_600) / 20_000)) * 1_000_000,
    );
  }
});

it("leaves trace 0 and the showcases alone", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 60 });
  expect(dataset.traces[0]?.duration_ns).toBe(60_000 * 1_000_000);
  for (const showcase of SHOWCASES) {
    expect(dataset.traces[showcase.index]?.duration_ns).toBe(showcase.spans[0].to * 1_000_000);
  }
});
