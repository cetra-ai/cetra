import { runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

async function main(): Promise<void> {
  // Qwen appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  const key = requiredEnv("OPENAI_API_KEY");
  await runAgent({
    bin: "qwen",
    args: [
      "--auth-type",
      "openai",
      "-m",
      "gpt-4o-mini",
      "--approval-mode",
      "yolo",
      "Print hello.txt with the shell and tell me what it says.",
    ],
    homeFiles: {
      ".qwen/settings.json": `${JSON.stringify(
        {
          telemetry: {
            enabled: true,
            target: "local",
            otlpEndpoint: otlpBase,
            otlpProtocol: "http",
            logPrompts: true,
            includeSensitiveSpanAttributes: true,
          },
          memory: {
            enableManagedAutoMemory: false,
            enableManagedAutoDream: false,
            enableAutoSkill: false,
          },
        },
        null,
        2,
      )}\n`,
      ".qwen/.env": `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20${key}\n`,
    },
    repoFiles: { "hello.txt": "hello from cetra\n" },
    env: {
      OPENAI_BASE_URL: mockUrl("/v1"),
      OPENAI_API_KEY: key,
      GOOGLE_API_KEY: "unused",
    },
  });
  writeMeta();
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
