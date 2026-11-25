import { createOpenAI } from "@ai-sdk/openai-v6";
import { Mastra } from "@mastra/core";
import { Agent } from "@mastra/core/agent";
import { createTool } from "@mastra/core/tools";
import { Observability } from "@mastra/observability";
import { OtelExporter } from "@mastra/otel-exporter";
import { z } from "zod";

import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

async function main(): Promise<void> {
  // The exporter owns its tracer provider and reads no OTEL_EXPORTER_OTLP_* variable,
  // so the scenario passes the runner's traces endpoint itself and skips setup().
  const openai = createOpenAI({
    baseURL: mockUrl("/v1"),
    apiKey: process.env.OPENAI_API_KEY,
  });
  const weather = createTool({
    id: "get_weather",
    description: "Look up the weather for a city",
    inputSchema: z.object({ city: z.string() }),
    execute: async ({ city }) => ({ temperature_c: 18, city }),
  });
  const agent = new Agent({
    id: "weather-agent",
    name: "weather-agent",
    instructions: "You are a weather assistant.",
    model: openai.chat("gpt-4o-mini"),
    tools: { get_weather: weather },
  });
  const mastra = new Mastra({
    agents: { agent },
    observability: new Observability({
      configs: {
        otel: {
          serviceName: requiredEnv("OTEL_SERVICE_NAME"),
          exporters: [
            new OtelExporter({
              provider: {
                custom: {
                  endpoint: requiredEnv("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"),
                  protocol: "http/protobuf",
                },
              },
              signals: { logs: false },
            }),
          ],
        },
      },
    }),
  });
  await mastra.getAgent("agent").generate("What's the weather in Paris?", { maxSteps: 3 });
  // shutdown flushes the batched spans before the process ends.
  await mastra.shutdown();
  writeMeta();
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.name : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
