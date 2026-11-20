import { agentPaths, runAgent } from "../../../agents.ts";
import { writeMeta } from "../../../harness.ts";

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined) {
    throw new Error(`${name} is unset`);
  }
  return value;
}

async function main(): Promise<void> {
  agentPaths();
  // Gemini appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = required("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  await runAgent({
    bin: "gemini",
    args: ["-p", "Run echo hello with the shell", "--yolo"],
    homeFiles: {
      ".gemini/settings.json": `${JSON.stringify(
        {
          telemetry: {
            enabled: true,
            traces: true,
            target: "local",
            otlpEndpoint: otlpBase,
            otlpProtocol: "http",
            logPrompts: true,
          },
          security: { auth: { selectedType: "gemini-api-key" } },
        },
        null,
        2,
      )}\n`,
    },
    env: {
      GEMINI_API_KEY: required("GEMINI_API_KEY"),
      GOOGLE_GEMINI_BASE_URL: required("CETRA_CAPTURE_MOCK_URL"),
      GEMINI_CLI_TRUST_WORKSPACE: "true",
    },
  });
  writeMeta();
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
