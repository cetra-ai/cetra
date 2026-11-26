import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

// cetra's extension (IN7.2), the file `cetra agents setup pi` writes into pi's extension folder.
const extensionSource = fileURLToPath(
  new URL("../../../../../../crates/cli/src/agents/pi_extension.ts", import.meta.url),
);

async function main(): Promise<void> {
  // The extension appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  const key = requiredEnv("OPENAI_API_KEY");
  await runAgent({
    bin: "pi",
    args: ["-p", "--model", "openai/gpt-4o-mini", "Read hello.txt and tell me what it says."],
    homeFiles: {
      ".pi/agent/models.json": `${JSON.stringify(
        {
          providers: {
            openai: {
              baseUrl: mockUrl("/v1"),
              api: "openai-completions",
              apiKey: key,
              models: [{ id: "gpt-4o-mini", type: "chat", name: "GPT 4o mini" }],
            },
          },
        },
        null,
        2,
      )}\n`,
      ".pi/agent/extensions/cetra.ts": readFileSync(extensionSource, "utf8"),
    },
    repoFiles: { "hello.txt": "hello from cetra\n" },
    env: {
      OTEL_EXPORTER_OTLP_ENDPOINT: otlpBase,
      OTEL_EXPORTER_OTLP_HEADERS: `Authorization=Bearer%20${key}`,
      OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT: "true",
    },
  });
  writeMeta();
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
