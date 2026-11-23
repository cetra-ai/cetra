import { createOpenAI } from "@ai-sdk/openai-v6";
import { Agent, VoltAgent, createTool } from "@voltagent/core";
import { z } from "zod";

import { finish, mockUrl, setup } from "../../../harness.ts";

async function main(): Promise<void> {
  // VoltAgent takes its tracer from the global provider that setup() registers.
  const provider = setup();
  try {
    const openai = createOpenAI({
      baseURL: mockUrl("/v1"),
      apiKey: process.env.OPENAI_API_KEY,
    });
    const weather = createTool({
      name: "get_weather",
      description: "Look up the weather for a city",
      parameters: z.object({ city: z.string() }),
      execute: async ({ city }) => ({ temperature_c: 18, city }),
    });
    const agent = new Agent({
      name: "weather-agent",
      instructions: "You are a weather assistant.",
      model: openai.chat("gpt-4o-mini"),
      tools: [weather],
    });
    new VoltAgent({ agents: { agent } });
    await agent.generateText("What's the weather in Paris?", {
      maxSteps: 3,
      userId: "user-1",
      conversationId: "conv-1",
    });
  } finally {
    await finish(provider);
  }
}

// VoltAgent keeps a handle open after the run, so the process exits explicitly either way.
main().then(
  () => process.exit(0),
  (error: unknown) => {
    const message = error instanceof Error ? error.name : "Error";
    process.stderr.write(`capture scenario failed: ${message}\n`);
    process.exit(1);
  },
);
