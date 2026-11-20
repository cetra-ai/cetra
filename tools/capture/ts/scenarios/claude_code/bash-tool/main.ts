import { runAgent } from "../../../agents.ts";
import { requiredEnv, writeMeta } from "../../../harness.ts";

async function main(): Promise<void> {
  await runAgent({
    bin: "claude",
    args: ["-p", "Run echo hello with Bash", "--allowedTools", "Bash", "--output-format", "text"],
    homeFiles: {},
    env: {
      ANTHROPIC_API_KEY: requiredEnv("ANTHROPIC_API_KEY"),
      ANTHROPIC_BASE_URL: requiredEnv("CETRA_CAPTURE_MOCK_URL"),
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
      DISABLE_AUTOUPDATER: "1",
      CLAUDE_CODE_ENABLE_TELEMETRY: "1",
      CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1",
      OTEL_TRACES_EXPORTER: "otlp",
      OTEL_LOGS_EXPORTER: "none",
      OTEL_METRICS_EXPORTER: "none",
      OTEL_EXPORTER_OTLP_PROTOCOL: "http/protobuf",
      OTEL_EXPORTER_OTLP_COMPRESSION: "none",
      OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT"),
      OTEL_TRACES_EXPORT_INTERVAL: "500",
      OTEL_METRICS_INCLUDE_REPOSITORY: "true",
      OTEL_LOG_USER_PROMPTS: "1",
      OTEL_LOG_TOOL_DETAILS: "1",
      OTEL_LOG_TOOL_CONTENT: "1",
    },
  });
  writeMeta();
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
