import path from "node:path";

import { runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned Oh My Pi npm build (IN7.9's § Versions).
const OMP_VERSION = "18.22.0";

async function main(): Promise<void> {
  // Oh My Pi appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  const key = requiredEnv("OPENAI_API_KEY");
  const tools = requiredEnv("CETRA_CAPTURE_TOOLS");
  const ompBin = path.join(tools, `oh-my-pi/${OMP_VERSION}/node_modules/.bin`);
  await runAgent({
    bin: path.join(ompBin, "omp"),
    args: [
      "-p",
      "--auto-approve",
      "--model",
      "openai/gpt-4o-mini",
      "Read hello.txt and tell me what it says.",
    ],
    homeFiles: {
      ".omp/agent/models.yml": `providers:\n  openai:\n    baseUrl: ${mockUrl("/v1")}\n    api: openai-completions\n    apiKey: ${key}\n    models:\n      - id: gpt-4o-mini\n        name: gpt-4o-mini\nmodelRoles:\n  default: openai/gpt-4o-mini\n`,
      ".omp/agent/.env": `OTEL_EXPORTER_OTLP_ENDPOINT=${otlpBase}\nOTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer ${key}\nOTEL_SERVICE_NAME=omp\nOTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true\nOTEL_LOGS_EXPORTER=none\nOTEL_METRICS_EXPORTER=none\n`,
    },
    repoFiles: { "hello.txt": "hello from cetra\n" },
    env: {
      // Bun must be found: run.env is applied after the runner's PATH.
      PATH: `${ompBin}:${process.env.PATH}`,
    },
  });
  writeMeta({
    "@oh-my-pi/pi-coding-agent": `../../../../target/capture-tools/oh-my-pi/${OMP_VERSION}/node_modules/@oh-my-pi/pi-coding-agent`,
  });
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
