import path from "node:path";

import { agentPaths, runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned Goose binary (IN7.9's § Versions).
const GOOSE_VERSION = "1.59.2";

async function main(): Promise<void> {
  // Goose appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  const key = requiredEnv("OPENAI_API_KEY");
  const { home } = agentPaths();
  await runAgent({
    bin: path.join(requiredEnv("CETRA_CAPTURE_TOOLS"), `goose/${GOOSE_VERSION}/goose`),
    args: [
      "run",
      "-t",
      "Read hello.txt and tell me what it says.",
      "--no-session",
      "--no-profile",
      "--with-builtin",
      "developer",
      "-q",
      "--output-format",
      "text",
    ],
    homeFiles: {
      ".config/goose/config.yaml":
        "GOOSE_PROVIDER: openai\nGOOSE_MODEL: gpt-4o-mini\nGOOSE_MODE: auto\n",
    },
    repoFiles: { "hello.txt": "hello from cetra\n" },
    env: {
      XDG_CONFIG_HOME: path.join(home, ".config"),
      OPENAI_API_KEY: key,
      OPENAI_BASE_URL: mockUrl("/v1"),
      GOOSE_PROVIDER: "openai",
      GOOSE_MODEL: "gpt-4o-mini",
      GOOSE_MODE: "auto",
      OTEL_EXPORTER_OTLP_ENDPOINT: otlpBase,
      OTEL_EXPORTER_OTLP_HEADERS: `Authorization=Bearer%20${key}`,
      OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT: "true",
      OTEL_LOGS_EXPORTER: "none",
      OTEL_METRICS_EXPORTER: "none",
    },
  });
  writeMeta({}, { goose: GOOSE_VERSION });
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
