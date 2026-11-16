export const PLACEHOLDER_KEY = "sk-cetra-…";

export const API_KEY_NOTE = "Create a secret key in Settings → API keys";

export const API_KEY_ASK_NOTE = "Ask a project admin for a secret key";

export const WAITING_LINE = "Waiting for the first trace…";

/** Raw OTLP/HTTP exporter settings. The endpoint is `<origin>/v1/traces`. */
export function otelSnippet(origin: string, key: string = PLACEHOLDER_KEY): string {
  return [
    `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=${origin}/v1/traces`,
    `OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer ${key}"`,
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf",
  ].join("\n");
}

/** `cetra.init` with the Python option names. Env vars apply when an argument is omitted. */
export function pythonSnippet(origin: string, key: string = PLACEHOLDER_KEY): string {
  return [
    "import cetra",
    "",
    "# Or set CETRA_API_KEY and CETRA_BASE_URL.",
    "cetra.init(",
    `    api_key="${key}",`,
    `    base_url="${origin}",`,
    ")",
  ].join("\n");
}

/** `cetra.init` with the TypeScript option names. The env vars match Python. */
export function typescriptSnippet(origin: string, key: string = PLACEHOLDER_KEY): string {
  return [
    'import * as cetra from "@cetra-ai/sdk";',
    "",
    "// Or set CETRA_API_KEY and CETRA_BASE_URL.",
    "cetra.init({",
    `  apiKey: "${key}",`,
    `  baseUrl: "${origin}",`,
    "});",
  ].join("\n");
}

/** Pass `cetra.aiTelemetry()` as the AI SDK's `experimental_telemetry`. */
export function vercelSnippet(origin: string, key: string = PLACEHOLDER_KEY): string {
  return [
    'import { generateText } from "ai";',
    'import * as cetra from "@cetra-ai/sdk";',
    "",
    "cetra.init({",
    `  apiKey: "${key}",`,
    `  baseUrl: "${origin}",`,
    "});",
    "",
    "await generateText({",
    "  model,",
    '  prompt: "Hello",',
    "  experimental_telemetry: cetra.aiTelemetry(),",
    "});",
  ].join("\n");
}
