import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { agentPrompt, CHECK_QUERY } from "./prompt.js";

const VALUES = {
  envFile: "/work/app/.env",
  tracesUrl: "http://localhost:8080/o/acme/p/demo/traces",
};

const QUERY_LINE = `   npx @cetra-ai/cli query "${CHECK_QUERY}" --since 15m --json`;

describe("agentPrompt", () => {
  it("returns the prompt text with the skill", () => {
    expect(agentPrompt({ ...VALUES, skill: true })).toBe(
      [
        "Add cetra tracing to this app.",
        "",
        "1. Read the cetra skill (.claude/skills/cetra or .agents/skills/cetra). If it is not there, read https://cetra.sh/llms.txt.",
        "2. Find the app's language and LLM libraries, and install the cetra SDK with the matching extras or instrumentations. Call init once at startup. The API key and URL are CETRA_API_KEY and CETRA_BASE_URL in /work/app/.env: load that file the way the app loads its settings. Never paste the key into code or chat.",
        "3. Run the app, or its tests, once so that it makes at least one LLM call. Flush before exit in short scripts.",
        "4. Check that traces arrived:",
        QUERY_LINE,
        "   If it returns no rows, follow the troubleshooting steps of the skill.",
        "5. Tell me what you changed, and give me this link: http://localhost:8080/o/acme/p/demo/traces",
      ].join("\n"),
    );
  });

  it("points at llms.txt alone without the skill", () => {
    const text = agentPrompt({ ...VALUES, skill: false });
    expect(text.split("\n")[2]).toBe("1. Read https://cetra.sh/llms.txt.");
    expect(text).not.toContain(".claude/skills/cetra");
    expect(text).toBe(
      agentPrompt({ ...VALUES, skill: true }).replace(
        "1. Read the cetra skill (.claude/skills/cetra or .agents/skills/cetra). If it is not there, read https://cetra.sh/llms.txt.",
        "1. Read https://cetra.sh/llms.txt.",
      ),
    );
  });
});

describe("CHECK_QUERY", () => {
  it("appears verbatim in the skill's step 5", async () => {
    const skill = await readFile(
      fileURLToPath(new URL("../../skills/cetra/SKILL.md", import.meta.url)),
      "utf8",
    );
    expect(skill).toContain(CHECK_QUERY);
  });
});
