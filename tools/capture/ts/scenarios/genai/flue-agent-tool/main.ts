import { readFileSync } from "node:fs";
import { createProvider } from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/api/openai-completions.lazy";
import { createOpenTelemetryInstrumentation } from "@flue/opentelemetry";
import { defineTool, init, instrument, setProvider, useModel, useTool } from "@flue/runtime";
import { start } from "@flue/runtime/node";
import * as v from "valibot";

import { finish, mockUrl, requiredEnv, setup } from "../../../harness.ts";

// The request model; the spans name it as gen_ai.request.model.
const MODEL = "gpt-4o-mini";

async function main(): Promise<void> {
  // The provider is registered with id "openai", so gen_ai.provider.name reads
  // "openai" exactly and the spans carry openai.api.type: pi-ai's provider ids
  // set both. An unregistered id would leave the spans unpriced.
  const openai = createProvider({
    id: "openai",
    auth: {
      apiKey: {
        name: "capture mock",
        resolve: async () => ({ auth: { apiKey: requiredEnv("OPENAI_API_KEY") } }),
      },
    },
    models: [
      {
        id: MODEL,
        name: MODEL,
        api: "openai-completions",
        provider: "openai",
        baseUrl: mockUrl("/v1"),
        reasoning: false,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 128000,
        maxTokens: 4096,
      },
    ],
    api: openAICompletionsApi(),
  });
  setProvider(openai);
  // The app owns the SDK (the harness's provider and exporter, as the Flue
  // docs require) and Flue's instrumentation exports through it.
  const provider = setup();
  try {
    const dispose = instrument(createOpenTelemetryInstrumentation());
    const readFile = defineTool({
      name: "read_file",
      description: "Read a text file and return its contents.",
      input: v.object({ path: v.string() }),
      run: async ({ data }) => ({ output: readFileSync(data.path, "utf8") }),
    });
    function Demo() {
      useModel(`openai/${MODEL}`);
      useTool(readFile);
      return "You are a helpful assistant. When asked to read a file, call read_file.";
    }
    const flue = await start({ agents: [Demo], env: { ...process.env }, providers: [] });
    try {
      const agent = init(Demo, { id: "capture-flue-1" });
      const receipt = await agent.dispatch(
        "Read hello.txt with the read_file tool and say what it says.",
      );
      const reply = await agent.read(receipt);
      if (reply.text.length === 0) {
        throw new Error("the run produced no text");
      }
    } finally {
      // stop drains in-flight work before the instrumentation is detached.
      await flue.stop();
      await dispose();
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
