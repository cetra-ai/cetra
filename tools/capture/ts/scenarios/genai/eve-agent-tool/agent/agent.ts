import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { defineAgent } from "eve";

// The mock plays the OpenAI Chat Completions API; the base URL comes from the
// scenario's environment. The provider name "openai" makes the chat spans read
// gen_ai.provider.name "openai.chat", which cetra maps back to "openai" and prices.
const openai = createOpenAICompatible({
  name: "openai",
  baseURL: `${process.env.CETRA_CAPTURE_MOCK_URL}/v1`,
  apiKey: process.env.OPENAI_API_KEY,
  includeUsage: true,
});

export default defineAgent({
  model: openai("gpt-4o-mini"),
  modelContextWindowTokens: 64_000,
});
