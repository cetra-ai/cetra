import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { agentPaths, runAgent } from "../../../agents.ts";
import { mockUrl, requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned OpenCode 1.x npm build (IN7.9's § Versions).
const OPENCODE_VERSION = "1.19.6";

// cetra's OpenCode plugin (IN7.11), the file `cetra agents setup opencode` writes.
const scenarioDir = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = path.resolve(scenarioDir, "../../../../../..");
const pluginSource = path.join(repoRoot, "crates/cli/src/agents/opencode_plugin.ts");

async function main(): Promise<void> {
  // The plugin appends /v1/traces itself, so the base is the endpoint without it.
  const otlpBase = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT").replace(/\/v1\/traces$/, "");
  const key = requiredEnv("OPENAI_API_KEY");
  const tools = requiredEnv("CETRA_CAPTURE_TOOLS");
  const { home } = agentPaths();
  await runAgent({
    bin: path.join(tools, `opencode/${OPENCODE_VERSION}/node_modules/.bin/opencode`),
    args: ["run", "--auto", "Read hello.txt and tell me what it says."],
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
        provider: { openai: { options: { baseURL: mockUrl("/v1"), apiKey: key } } },
        model: "openai/gpt-4o-mini",
      })}\n`,
      "hello.txt": "hello from cetra\n",
    },
    env: {
      OPENCODE_DISABLE_AUTOUPDATE: "1",
      OPENCODE_DISABLE_MODELS_FETCH: "1",
      OPENCODE_DISABLE_LSP_DOWNLOAD: "1",
      OPENCODE_DISABLE_DEFAULT_PLUGINS: "1",
      OPENAI_API_KEY: key,
      // OpenCode 1.x keeps its database under the passwd home unless XDG_DATA_HOME is
      // set: without it the capture would read the operator's own database.
      XDG_CONFIG_HOME: path.join(home, ".config"),
      XDG_DATA_HOME: path.join(home, ".local", "share"),
    },
  });
  writeMeta({
    "opencode-ai": `../../../../target/capture-tools/opencode/${OPENCODE_VERSION}/node_modules/opencode-ai`,
  });
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
