/** The check step's statement. It is the same as skills/cetra/SKILL.md's step 5 (a test checks it). */
export const CHECK_QUERY =
  "SELECT trace_id, name, start_time, span_count, llm_count, error_count FROM traces ORDER BY start_time DESC LIMIT 5";

/** The text `init` prints for the coding agent. */
export function agentPrompt(values: {
  envFile: string;
  tracesUrl: string;
  skill: boolean;
}): string {
  const step1 = values.skill
    ? "1. Read the cetra skill (.claude/skills/cetra or .agents/skills/cetra). If it is not there, read https://cetra.sh/llms.txt."
    : "1. Read https://cetra.sh/llms.txt.";
  return [
    "Add cetra tracing to this app.",
    "",
    step1,
    "2. Find the app's language and LLM libraries, and install the cetra SDK with the matching extras or instrumentations. Call init once at startup. The API key and URL are CETRA_API_KEY and CETRA_BASE_URL in " +
      `${values.envFile}: load that file the way the app loads its settings. Never paste the key into code or chat.`,
    "3. Run the app, or its tests, once so that it makes at least one LLM call. Flush before exit in short scripts.",
    "4. Check that traces arrived:",
    `   npx @cetra-ai/cli query "${CHECK_QUERY}" --since 15m --json`,
    "   If it returns no rows, follow the troubleshooting steps of the skill.",
    `5. Tell me what you changed, and give me this link: ${values.tracesUrl}`,
  ].join("\n");
}
