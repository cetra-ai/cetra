import { expect, it } from "vitest";
import {
  GO_TARGETS,
  goDestination,
  goKeyOf,
  hexId,
  isTypingTarget,
  matchesBinding,
  reduceShortcut,
  SHORTCUTS,
} from "@/lib/shortcuts";

const plain = { metaKey: false, ctrlKey: false, altKey: false };

it("lists every shortcut the help dialog shows", () => {
  expect(SHORTCUTS.map((row) => row.keys)).toEqual([
    "/",
    "f",
    "j / k, ↓ / ↑",
    "Enter",
    "Esc",
    "t",
    "0",
    "⌘↵",
    "⌘↵",
    "⌘⇧↵",
    "g then a letter",
    "⌘K",
    "?",
  ]);
});

it("recognizes keys, chords, and modifiers", () => {
  expect(reduceShortcut(null, { ...plain, key: "j" }, false)).toEqual({
    chord: null,
    action: "row-next",
  });
  expect(reduceShortcut(null, { ...plain, key: "ArrowUp" }, false).action).toBe("row-prev");
  expect(reduceShortcut(null, { ...plain, key: "?" }, false).action).toBe("help");
  expect(reduceShortcut(null, { ...plain, key: "g" }, false)).toEqual({
    chord: "g",
    action: null,
  });
  expect(reduceShortcut("g", { ...plain, key: "p" }, false).action).toBe("go-spans");
  expect(reduceShortcut("g", { ...plain, key: "s" }, false).action).toBe("go-sessions");
  expect(reduceShortcut("g", { ...plain, key: "e" }, false).action).toBe("go-evaluators");
  expect(reduceShortcut("g", { ...plain, key: "i" }, false).action).toBe("go-datasets");
  expect(reduceShortcut("g", { ...plain, key: "x" }, false).action).toBe("go-experiments");
  expect(reduceShortcut("g", { ...plain, key: "m" }, false).action).toBe("go-prompts");
  expect(reduceShortcut("g", { ...plain, key: "l" }, false).action).toBe("go-playground");
  expect(reduceShortcut("g", { ...plain, key: "r" }, false).action).toBe("go-annotation-queues");
  expect(reduceShortcut("g", { ...plain, key: "d" }, false).action).toBe("go-dashboard");
  expect(reduceShortcut("g", { ...plain, key: "j" }, false)).toEqual({
    chord: null,
    action: "row-next",
  });
  expect(reduceShortcut(null, { ...plain, key: "k", metaKey: true }, true)).toEqual({
    chord: null,
    action: "palette",
  });
  expect(reduceShortcut(null, { ...plain, key: "k", ctrlKey: true }, false).action).toBe("palette");
  expect(reduceShortcut("g", { ...plain, key: "s" }, true)).toEqual({
    chord: null,
    action: null,
  });
  expect(goDestination("go-users")).toBe("/$org/$project/users");
  expect(goDestination("go-alerts")).toBe("/$org/$project/alerts");
  expect(matchesBinding({ ...plain, key: "0" }, "fit")).toBe(true);
  expect(matchesBinding({ ...plain, key: "k", metaKey: true }, "row-prev")).toBe(false);
});

it("gives every project page its go letter and back again", () => {
  expect(goKeyOf("/$org/$project/spans")).toBe("p");
  expect(goKeyOf("/$org/$project/sessions")).toBe("s");
  expect(goKeyOf("/$org/$project/experiments")).toBe("x");
  expect(goKeyOf("/$org/$project/prompts")).toBe("r");
  expect(goKeyOf("/$org/$project/playground")).toBe("l");
  expect(goKeyOf("/$org/$project/settings/general")).toBeUndefined();
  for (const letter of Object.keys(GO_TARGETS)) {
    const target = GO_TARGETS[letter as keyof typeof GO_TARGETS];
    expect(goKeyOf(target.to)).toBe(letter);
  }
});

it("accepts a trace or span id with dashes and any case", () => {
  expect(hexId("A1000000000000000000000000000001", 32)).toBe("a1000000000000000000000000000001");
  expect(hexId("a1000000-0000-0000-0000-000000000001", 32)).toBe(
    "a1000000000000000000000000000001",
  );
  expect(hexId("B100000000000013", 16)).toBe("b100000000000013");
  expect(hexId("b100000000000013", 32)).toBeUndefined();
  expect(hexId("not-an-id", 32)).toBeUndefined();
});

it("treats fields and the code editor as typing", () => {
  const input = document.createElement("input");
  const editor = document.createElement("div");
  editor.className = "cm-editor";
  const child = document.createElement("span");
  editor.append(child);
  document.body.append(input, editor);
  expect(isTypingTarget(input)).toBe(true);
  expect(isTypingTarget(child)).toBe(true);
  expect(isTypingTarget(document.body)).toBe(false);
  input.remove();
  editor.remove();
});
