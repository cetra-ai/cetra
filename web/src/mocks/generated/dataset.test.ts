import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset, type Dataset } from "@/mocks/generated/dataset";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");

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
