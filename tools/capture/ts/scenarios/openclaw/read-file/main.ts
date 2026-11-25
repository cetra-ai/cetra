import path from "node:path";

import { agentPaths, runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned OpenClaw tool project (IN7.0). Its staged home holds the diagnostics-otel plugin
// that `just capture-tools` installed, so the capture never writes to the operator's ~/.openclaw.
const openclawDir = path.join(requiredEnv("CETRA_CAPTURE_TOOLS"), "openclaw/2026.11.5");

async function main(): Promise<void> {
  // The plugin appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  const key = requiredEnv("OPENAI_API_KEY");
  await runAgent({
    bin: path.join(openclawDir, "node_modules/.bin/openclaw"),
    args: ["agent", "--local", "--json", "-m", "Read hello.txt and tell me what it says."],
    homeDirs: { ".openclaw": path.join(openclawDir, "home/.openclaw") },
    homeFiles: {
      ".openclaw/openclaw.json": `${JSON.stringify(
        {
          agents: {
            defaults: {
              model: { primary: "openai/gpt-4o-mini" },
              workspace: agentPaths().repo,
            },
          },
          models: {
            providers: {
              openai: {
                baseUrl: mockUrl("/v1"),
                api: "openai-completions",
                apiKey: key,
                models: [{ id: "gpt-4o-mini", name: "GPT 4o mini" }],
              },
            },
          },
          plugins: {
            allow: ["diagnostics-otel"],
            entries: { "diagnostics-otel": { enabled: true } },
          },
          diagnostics: {
            enabled: true,
            otel: {
              enabled: true,
              endpoint: otlpBase,
              protocol: "http/protobuf",
              serviceName: "openclaw",
              traces: true,
              metrics: false,
              logs: false,
              headers: { Authorization: `Bearer ${key}` },
              captureContent: true,
              flushIntervalMs: 2000,
            },
          },
        },
        null,
        2,
      )}\n`,
    },
    repoFiles: { "hello.txt": "hello from cetra\n" },
    env: {},
  });
  // The plugin's version is OpenClaw's (IN7.0): the alias resolves from tools/capture/ts/node_modules.
  writeMeta({
    openclaw: "../../../../target/capture-tools/openclaw/2026.11.5/node_modules/openclaw",
  });
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
