import { expect, it } from "vitest";
import { adapterOf, DEFAULT_BASE_URLS, providerChoice, providerLabel } from "@/lib/llm-adapters";

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

it("saves OpenAI-compatible as the openai adapter", () => {
  expect(adapterOf("openai")).toBe("openai");
  expect(adapterOf("openai_compatible")).toBe("openai");
  expect(adapterOf("anthropic")).toBe("anthropic");
  expect(adapterOf("gemini")).toBe("gemini");
});

it("knows each adapter's default base URL", () => {
  expect(DEFAULT_BASE_URLS).toEqual({
    openai: "https://api.openai.com/v1",
    anthropic: "https://api.anthropic.com",
    gemini: "https://generativelanguage.googleapis.com",
  });
});
