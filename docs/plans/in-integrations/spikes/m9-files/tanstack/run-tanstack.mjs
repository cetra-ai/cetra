// M9 spike: TanStack AI chat with a tool, traced by the TanStack OTel middleware (mode "otel")
// or the OpenInference middleware (mode "oi"). Fake key, base URL read from ./base.url (loopback only).
import { readFileSync } from "node:fs";
import { chat, toolDefinition } from "@tanstack/ai";
import { openaiText } from "@tanstack/ai-openai";
import { otelMiddleware } from "@tanstack/ai/middlewares/otel";
import { openInferenceMiddleware } from "@arizeai/openinference-tanstack-ai";
import { trace } from "@opentelemetry/api";
import { NodeTracerProvider } from "@opentelemetry/sdk-trace-node";
import { SimpleSpanProcessor } from "@opentelemetry/sdk-trace-base";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
import { z } from "zod";

const mode = process.env.M9_MODE ?? "otel";
const base = readFileSync("base.url", "utf8").trim();
const exporter = new OTLPTraceExporter({ url: `${base}/v1/traces` });
const provider = new NodeTracerProvider({ spanProcessors: [new SimpleSpanProcessor(exporter)] });
provider.register();
const tracer = trace.getTracer("m9-tanstack");

const readFile = toolDefinition({
  name: "read_file",
  description: "Read a file from disk",
  inputSchema: z.object({ path: z.string() }),
  outputSchema: z.object({ content: z.string() }),
}).server(async ({ path }) => ({ content: `hello from ${path}` }));

const middleware = mode === "oi"
  ? [openInferenceMiddleware({ tracer })]
  : [otelMiddleware({ tracer, captureContent: process.env.M9_CAPTURE === "1" })];

const stream = chat({
  adapter: openaiText("gpt-4o-mini", { baseURL: `${base}/v1` }),
  messages: [{ role: "user", content: "Read hello.txt and tell me what it says." }],
  threadId: "m9-thread-1",
  tools: [readFile],
  middleware,
});
let text = "";
for await (const chunk of stream) {
  if (chunk.type === "TEXT_MESSAGE_CONTENT" || chunk.type === "content") text += chunk.delta ?? chunk.content ?? "";
}
console.log("assistant text:", text.slice(0, 200));
await provider.forceFlush();
await provider.shutdown();
