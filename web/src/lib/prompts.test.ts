import { describe, expect, it } from "vitest";
import { problemResponse } from "@/test/problem";
import {
  asChat,
  emptyChat,
  fieldPositions,
  isValidLabel,
  isValidPromptName,
  labelChoices,
  parsePromptSearch,
  parsePromptsSearch,
  tagChoices,
  templateText,
  withPromptTab,
  withPromptVersion,
} from "./prompts";

describe("parsePromptsSearch", () => {
  it("keeps trimmed, non-empty text and drops blank text", () => {
    expect(parsePromptsSearch({ q: "  supp ", label: "staging", tag: "" })).toEqual({
      q: "supp",
      label: "staging",
    });
    expect(parsePromptsSearch({ q: "   " })).toEqual({});
  });

  it("drops text longer than 200 characters", () => {
    expect(parsePromptsSearch({ q: "a".repeat(201) })).toEqual({});
    expect(parsePromptsSearch({ q: "a".repeat(200) })).toEqual({ q: "a".repeat(200) });
  });

  it("reads create only for 1, '1' and true", () => {
    expect(parsePromptsSearch({ create: 1 })).toEqual({ create: true });
    expect(parsePromptsSearch({ create: "1" })).toEqual({ create: true });
    expect(parsePromptsSearch({ create: true })).toEqual({ create: true });
    expect(parsePromptsSearch({ create: "0" })).toEqual({});
    expect(parsePromptsSearch({ create: "yes" })).toEqual({});
  });
});

describe("parsePromptSearch", () => {
  it("accepts the four tabs and drops other values", () => {
    expect(parsePromptSearch({ tab: "versions" })).toEqual({ tab: "versions" });
    expect(parsePromptSearch({ tab: "metrics" })).toEqual({ tab: "metrics" });
    expect(parsePromptSearch({ tab: "other" })).toEqual({});
  });

  it("reads a version of 1 or more from a number or a digit string", () => {
    expect(parsePromptSearch({ version: 3 })).toEqual({ version: 3 });
    expect(parsePromptSearch({ version: "2" })).toEqual({ version: 2 });
    expect(parsePromptSearch({ version: 0 })).toEqual({});
    expect(parsePromptSearch({ version: "-1" })).toEqual({});
    expect(parsePromptSearch({ version: 1.5 })).toEqual({});
    expect(parsePromptSearch({ version: "v2" })).toEqual({});
  });
});

describe("isValidPromptName", () => {
  it("follows the D-V-2 name rule", () => {
    expect(isValidPromptName("support-reply")).toBe(true);
    expect(isValidPromptName("v1.2_draft")).toBe(true);
    expect(isValidPromptName("123")).toBe(true);
    expect(isValidPromptName("new")).toBe(true);
    expect(isValidPromptName("a".repeat(200))).toBe(true);
    expect(isValidPromptName("a".repeat(201))).toBe(false);
    expect(isValidPromptName("-leading")).toBe(false);
    expect(isValidPromptName("has space")).toBe(false);
    expect(isValidPromptName("")).toBe(false);
  });
});

describe("isValidLabel", () => {
  it("follows the D-V-8 label rule and refuses latest and all digits", () => {
    expect(isValidLabel("production")).toBe(true);
    expect(isValidLabel("canary.2")).toBe(true);
    expect(isValidLabel("latest")).toBe(false);
    expect(isValidLabel("123")).toBe(false);
    expect(isValidLabel("Prod")).toBe(false);
    expect(isValidLabel("a".repeat(65))).toBe(false);
  });
});

describe("asChat and templateText", () => {
  const chat = [
    { role: "system", content: "You help." },
    { type: "placeholder", name: "history" },
    { role: "user", content: "Question" },
  ];

  it("keeps the entries that fit and drops the rest", () => {
    expect(asChat([...chat, { role: "tool", content: "x" }, "text", null])).toEqual(chat);
    expect(asChat("not a list")).toEqual([]);
  });

  it("writes a chat template as one block per entry", () => {
    expect(templateText(chat, "chat")).toBe(
      "[system]\nYou help.\n[placeholder: history]\n[user]\nQuestion",
    );
  });

  it("writes a text template as its string, and nothing for other values", () => {
    expect(templateText("Hello {{ name }}", "text")).toBe("Hello {{ name }}");
    expect(templateText(chat, "text")).toBe("");
  });

  it("starts a new chat with a system and a user message", () => {
    expect(emptyChat()).toEqual([
      { role: "system", content: "" },
      { role: "user", content: "" },
    ]);
  });
});

describe("fieldPositions", () => {
  it("keeps the first position of each field", () => {
    const error = {
      status: 400,
      title: "Bad Request",
      code: "bad_request",
      errors: [
        { field: "template", message: "a", position: { line: 2, column: 5 } },
        { field: "template", message: "b", position: { line: 9, column: 1 } },
        { field: "config", message: "c" },
      ],
    };
    expect(fieldPositions(error)).toEqual({ template: { line: 2, column: 5 } });
  });

  it("reads positions from a problem response body", async () => {
    const response = problemResponse(400, "bad_request", {
      errors: [{ field: "template[0].role", message: "x", position: { line: 1, column: 1 } }],
    });
    const body = await response.json();
    expect(fieldPositions(body)).toEqual({ "template[0].role": { line: 1, column: 1 } });
  });

  it("is empty for anything that is not a problem", () => {
    expect(fieldPositions(new Error("network"))).toEqual({});
    expect(fieldPositions(undefined)).toEqual({});
  });
});

describe("labelChoices and tagChoices", () => {
  const rows = [
    { labels: [{ label: "production" }, { label: "staging" }], tags: ["support"] },
    { labels: [{ label: "canary" }], tags: ["billing", "support"] },
  ];

  it("offers latest and production first, then the other labels sorted", () => {
    expect(labelChoices(rows, ["production", "prod"], undefined)).toEqual([
      "latest",
      "production",
      "canary",
      "prod",
      "staging",
    ]);
  });

  it("keeps the selected label even when no loaded row has it", () => {
    expect(labelChoices([], [], "archived")).toEqual(["latest", "production", "archived"]);
  });

  it("offers the loaded tags and the selected tag, sorted, once each", () => {
    expect(tagChoices(rows, undefined)).toEqual(["billing", "support"]);
    expect(tagChoices([], "ops")).toEqual(["ops"]);
  });
});

describe("prompt page search", () => {
  it("sets the version, and drops it for the latest", () => {
    expect(withPromptVersion({ tab: "versions" }, 2)).toEqual({ tab: "versions", version: 2 });
    expect(withPromptVersion({ tab: "versions", version: 2 }, undefined)).toEqual({
      tab: "versions",
    });
  });

  it("sets the tab and keeps the version", () => {
    expect(withPromptTab({ version: 1 }, "labels")).toEqual({ version: 1, tab: "labels" });
  });
});
