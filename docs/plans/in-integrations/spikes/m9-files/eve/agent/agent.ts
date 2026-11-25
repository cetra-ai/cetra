import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { defineAgent } from "eve";

// M9 spike: the model is the loopback fake (OpenAI chat completions). Base URL comes from the run script.
const fake = createOpenAICompatible({
  name: "fake",
  baseURL: process.env.M9_OPENAI_BASE!,
  apiKey: "sk-fake-m9",
  includeUsage: true,
});

export default defineAgent({
  model: fake("gpt-4o-mini"),
  modelContextWindowTokens: 64_000,
});
