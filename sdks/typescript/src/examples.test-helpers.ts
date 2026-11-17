/**
 * The tested examples of the README and the docs site (T13, D-W-6). Each
 * function marked with a `// example:<name>` comment has twin blocks in
 * README.md (preceded by an `example:<name>` HTML comment) and in the site
 * pages (preceded by an `example:<name>` MDX comment). examples.test.ts runs
 * every function against `startMemory` and checks the texts are
 * byte-identical (the body, dedented). Bodies therefore contain no `import`
 * statements: the pages show imports once, above the first block.
 *
 * Not part of the build (no entry imports this file); type-checked by
 * tsconfig.test.json only.
 */

import { aiTelemetry } from "./ai.js";
import { withContext } from "./context.js";
import { withLlmSpan } from "./llm.js";
import { flush, init } from "./runtime.js";
import { score } from "./scores.js";
import { observe } from "./span.js";

// example:quick-start
export async function quickStart(): Promise<void> {
  init({ apiKey: process.env.CETRA_API_KEY });
  const searchDocs = observe(({ query }: { query: string }) => `results for ${query}`, {
    kind: "tool",
    name: "search_docs",
  });
  searchDocs({ query: "pricing" });
  await flush();
}

// example:with-llm-span
export function withLlmSpanExample(): void {
  withLlmSpan(
    "chat",
    {
      provider: "openai",
      model: "gpt-4o-mini",
      inputMessages: [{ role: "user", content: "hi" }],
      params: { temperature: 0.2 },
    },
    (span) => {
      span.setOutputMessages([{ role: "assistant", content: "hello" }]);
      span.setUsage({ inputTokens: 12, outputTokens: 3 });
    },
  );
}

// example:with-context
export function withContextExample(): void {
  const turn = observe(({ question }: { question: string }) => `answered: ${question}`, {
    name: "turn",
  });
  withContext({ sessionId: "chat-42", userId: "u-17", tags: ["beta"] }, () =>
    turn({ question: "why?" }),
  );
}

// example:score
export function scoreExample(): string | undefined {
  // The returned id is yours to keep: pass it back as `scoreId` to update.
  const id = score({ name: "helpful", value: 0.8, sessionId: "chat-42", reason: "human review" });
  return id;
}

// example:ai-telemetry
export function aiTelemetryExample(): void {
  // Pass `telemetry` as `experimental_telemetry` to the Vercel AI SDK; the
  // tracer it hands out is the cetra one, so AI spans land in cetra.
  const telemetry = aiTelemetry({ sessionId: "chat-42", functionId: "chat" });
  telemetry.tracer?.startSpan("ai.chat").end();
}
