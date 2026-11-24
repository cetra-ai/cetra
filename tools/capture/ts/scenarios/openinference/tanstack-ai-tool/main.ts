import { readFileSync } from "node:fs";
import { openInferenceMiddleware } from "@arizeai/openinference-tanstack-ai";
import { trace } from "@opentelemetry/api";
import { chat, toolDefinition } from "@tanstack/ai";
import { openaiText } from "@tanstack/ai-openai";
import { z } from "zod";

import { finish, mockUrl, setup } from "../../../harness.ts";

// The request model; the spans name it as llm.model_name.
const MODEL = "gpt-4o-mini";

async function main(): Promise<void> {
  const provider = setup();
  try {
    // The middleware emits through the tracer it is given, here one of the
    // harness's registered provider.
    const tracer = trace.getTracer("capture-tanstack-ai");
    const readFile = toolDefinition({
      name: "read_file",
      description: "Read a file from disk",
      inputSchema: z.object({ path: z.string() }),
      outputSchema: z.object({ content: z.string() }),
    }).server(async ({ path }) => ({ content: readFileSync(path, "utf8") }));
    // The OpenAI adapter calls the Responses API, streamed.
    const stream = chat({
      adapter: openaiText(MODEL, { baseURL: mockUrl("/v1") }),
      messages: [{ role: "user", content: "Read hello.txt and tell me what it says." }],
      threadId: "capture-thread-1",
      tools: [readFile],
      middleware: [openInferenceMiddleware({ tracer })],
    });
    // Consuming the stream drives the run: both model calls and the tool
    // execute while it is iterated, and the middleware ends every span itself.
    let text = "";
    for await (const chunk of stream) {
      if (chunk.type === "TEXT_MESSAGE_CONTENT") {
        text += chunk.delta;
      }
    }
    if (text.length === 0) {
      throw new Error("the run produced no text");
    }
  } finally {
    await finish(provider);
  }
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.name : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
