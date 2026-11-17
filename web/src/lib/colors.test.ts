import { expect, it } from "vitest";
import {
  CATEGORY_CLASSES,
  categoryClasses,
  categoryIndex,
  environmentClasses,
  modelProvider,
  providerClasses,
} from "@/lib/colors";

it("gives every category its own full class names", () => {
  expect(CATEGORY_CLASSES).toHaveLength(8);
  expect(CATEGORY_CLASSES[0]).toEqual({
    text: "text-cat-1",
    dot: "bg-cat-1",
    pill: "pill [--pill:var(--cat-1)]",
  });
  expect(CATEGORY_CLASSES[7]).toEqual({
    text: "text-cat-8",
    dot: "bg-cat-8",
    pill: "pill [--pill:var(--cat-8)]",
  });
});

it("hashes names to stable concrete indices (FNV-1a, modulo 8)", () => {
  expect(categoryIndex("a")).toBe(4);
  expect(categoryIndex("foo")).toBe(7);
  expect(categoryIndex("production")).toBe(2);
  expect(categoryIndex("")).toBe(5);
  expect(categoryIndex("production")).toBe(categoryIndex("production"));
  expect(categoryClasses("a")).toBe(CATEGORY_CLASSES[4]);
});

it("maps each well-known model prefix to its provider", () => {
  expect(modelProvider("claude-3-5-sonnet")).toBe("anthropic");
  expect(modelProvider("Claude-Opus")).toBe("anthropic");
  expect(modelProvider("gpt-4o")).toBe("openai");
  expect(modelProvider("o1-mini")).toBe("openai");
  expect(modelProvider("o3")).toBe("openai");
  expect(modelProvider("o4-mini")).toBe("openai");
  expect(modelProvider("text-embedding-3-small")).toBe("openai");
  expect(modelProvider("gemini-2.0-flash")).toBe("google");
  expect(modelProvider("gemma-2-9b")).toBe("google");
  expect(modelProvider("mistral-large")).toBe("mistral");
  expect(modelProvider("mixtral-8x7b")).toBe("mistral");
  expect(modelProvider("codestral-latest")).toBe("mistral");
  expect(modelProvider("llama-3.1-70b")).toBe("meta");
  expect(modelProvider("command-r-plus")).toBe("cohere");
  expect(modelProvider("deepseek-chat")).toBe("deepseek");
});

it("falls back to the text before the first dash or slash", () => {
  expect(modelProvider("qwen/qwen2.5")).toBe("qwen");
  expect(modelProvider("phi-3-mini")).toBe("phi");
  expect(modelProvider("phi3")).toBe("phi3");
});

it("gives the fixed providers their fixed colours", () => {
  expect(providerClasses("claude-3-haiku")).toBe(CATEGORY_CLASSES[5]);
  expect(providerClasses("gpt-4o")).toBe(CATEGORY_CLASSES[1]);
  expect(providerClasses("gemini-pro")).toBe(CATEGORY_CLASSES[0]);
  expect(providerClasses("mistral-large")).toBe(CATEGORY_CLASSES[2]);
  expect(providerClasses("llama-3")).toBe(CATEGORY_CLASSES[3]);
  expect(providerClasses("command-r")).toBe(CATEGORY_CLASSES[4]);
  expect(providerClasses("deepseek-chat")).toBe(CATEGORY_CLASSES[7]);
});

it("falls back to the hash for an unknown provider", () => {
  expect(providerClasses("qwen-7b")).toBe(categoryClasses("qwen"));
  expect(providerClasses("qwen-7b")).toBe(CATEGORY_CLASSES[categoryIndex("qwen")]);
});

it("gives production the first colour and keeps it from every other environment", () => {
  expect(environmentClasses("production")).toBe(CATEGORY_CLASSES[0]);
  expect(environmentClasses("Prod")).toBe(CATEGORY_CLASSES[0]);
  for (const name of ["staging", "dev", "qa", "local", "preview", "eu-west"]) {
    expect(environmentClasses(name)).not.toBe(CATEGORY_CLASSES[0]);
  }
});
