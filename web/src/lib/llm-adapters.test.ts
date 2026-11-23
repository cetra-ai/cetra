import { expect, it } from "vitest";
import { providerChoice, providerLabel } from "@/lib/llm-adapters";

it("maps a connection to its provider choice", () => {
  expect(providerChoice({ adapter: "openai" })).toBe("openai");
  expect(providerChoice({ adapter: "openai", base_url: "" })).toBe("openai");
  expect(providerChoice({ adapter: "openai", base_url: "https://gw.example.com/v1" })).toBe(
    "openai_compatible",
  );
  expect(providerChoice({ adapter: "anthropic" })).toBe("anthropic");
  expect(providerChoice({ adapter: "gemini" })).toBe("gemini");
});

it("labels each provider choice", () => {
  expect(providerLabel({ adapter: "openai" })).toBe("OpenAI");
  expect(providerLabel({ adapter: "openai", base_url: "" })).toBe("OpenAI");
  expect(providerLabel({ adapter: "openai", base_url: "https://gw.example.com/v1" })).toBe(
    "OpenAI-compatible",
  );
  expect(providerLabel({ adapter: "anthropic" })).toBe("Anthropic");
  expect(providerLabel({ adapter: "gemini" })).toBe("Gemini");
});
