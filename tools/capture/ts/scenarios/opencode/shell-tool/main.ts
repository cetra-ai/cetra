import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { agentPaths, runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned OpenCode v2 binary (IN7.9's § Versions).
const OPENCODE_VERSION = "2.3.0";

// cetra's OpenCode plugin (IN7.11), the file `cetra agents setup opencode` writes.
const scenarioDir = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = path.resolve(scenarioDir, "../../../../../..");
const pluginSource = path.join(repoRoot, "crates/cli/src/agents/opencode_plugin.ts");

async function main(): Promise<void> {
  // The plugin appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  const key = requiredEnv("OPENAI_API_KEY");
  const { home } = agentPaths();
  await runAgent({
    bin: path.join(requiredEnv("CETRA_CAPTURE_TOOLS"), `opencode-v2/${OPENCODE_VERSION}/opencode`),
    args: ["run", "--standalone", "--auto", "Read hello.txt and tell me what it says."],
    homeFiles: {
      ".config/opencode/plugins/cetra-opencode.ts": readFileSync(pluginSource, "utf8"),
      ".config/opencode/cetra-opencode.json": `${JSON.stringify(
        {
          managed_by: "cetra agents setup opencode",
          endpoint: otlpBase,
          headers: { Authorization: `Bearer ${key}` },
          capture_content: true,
        },
        null,
        2,
      )}\n`,
    },
    repoFiles: {
      "opencode.json": `${JSON.stringify({
        $schema: "https://opencode.ai/config.json",
        model: "openai/gpt-4o-mini",
        providers: { openai: { settings: { baseURL: mockUrl("/v1") } } },
      })}\n`,
      "hello.txt": "hello from cetra\n",
    },
    env: {
      OPENCODE_DISABLE_AUTOUPDATE: "1",
      OPENCODE_DISABLE_MODELS_FETCH: "1",
      OPENCODE_DISABLE_LSP_DOWNLOAD: "1",
      OPENCODE_DISABLE_DEFAULT_PLUGINS: "1",
      OPENAI_API_KEY: key,
      XDG_CONFIG_HOME: path.join(home, ".config"),
    },
  });
  writeMeta({}, { opencode: OPENCODE_VERSION });
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
