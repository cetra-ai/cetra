import path from "node:path";

import { agentPaths, runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned Hermes Agent environment and telemetry plugin (IN7.9's § Versions).
const HERMES_AGENT_VERSION = "0.23.2";
const HERMES_OTEL_VERSION = "1.33.0";

async function main(): Promise<void> {
  // hermes-otel posts to the endpoint as configured, so it takes the full traces URL.
  const traces = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT");
  const key = requiredEnv("OPENAI_API_KEY");
  const { home } = agentPaths();
  await runAgent({
    bin: path.join(requiredEnv("CETRA_CAPTURE_TOOLS"), `hermes/${HERMES_AGENT_VERSION}/bin/hermes`),
    args: [
      "chat",
      "-q",
      "Read hello.txt and tell me what it says.",
      "--oneshot",
      "--yolo",
      "--provider",
      "custom",
      "--base-url",
      mockUrl("/v1"),
      "--api-key",
      key,
      "-m",
      "gpt-4o-mini",
    ],
    homeFiles: {
      ".hermes/config.yaml": `model:\n  provider: "custom"\n  base_url: "${mockUrl("/v1")}"\n  api_key: "${key}"\n  default: "gpt-4o-mini"\n  streaming: true\nagent:\n  toolsets: ["base"]\ndisplay:\n  telemetry: false\nplugins:\n  enabled: [hermes_otel]\n`,
      ".hermes/hermes_otel.yaml": `project_name: cetra-capture\nbackends:\n  - type: otlp\n    endpoint: ${traces}\n    headers:\n      Authorization: "Bearer ${key}"\n    metrics: false\ncontent_capture: full\ncapture_logs: false\n`,
    },
    repoFiles: { "hello.txt": "hello from cetra\n" },
    env: {
      HERMES_HOME: path.join(home, ".hermes"),
    },
  });
  writeMeta({}, { "hermes-agent": HERMES_AGENT_VERSION, "hermes-otel": HERMES_OTEL_VERSION });
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
