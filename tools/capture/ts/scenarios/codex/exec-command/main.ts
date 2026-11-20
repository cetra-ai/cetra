import path from "node:path";

import { agentPaths, runAgent } from "../../../agents.ts";
import { mockUrl, writeMeta } from "../../../harness.ts";

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined) {
    throw new Error(`${name} is unset`);
  }
  return value;
}

async function main(): Promise<void> {
  const { home } = agentPaths();
  const otlpEndpoint = required("CETRA_CAPTURE_OTLP_ENDPOINT");
  await runAgent({
    bin: "codex",
    args: [
      "exec",
      "--skip-git-repo-check",
      "--dangerously-bypass-approvals-and-sandbox",
      "Run echo hello",
    ],
    homeFiles: {
      ".codex/config.toml": `model = "gpt-5.5-codex"
model_provider = "mock"

[model_providers.mock]
name = "Mock"
base_url = "${mockUrl("/v1")}"
env_key = "MOCK_API_KEY"
wire_api = "responses"

[otel]
environment = "capture"
exporter = "none"
metrics_exporter = "none"
trace_exporter = { otlp-http = { endpoint = "${otlpEndpoint}", protocol = "binary" } }

[analytics]
enabled = false
`,
    },
    env: {
      CODEX_HOME: path.join(home, ".codex"),
      MOCK_API_KEY: required("OPENAI_API_KEY"),
    },
  });
  writeMeta();
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
