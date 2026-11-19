import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { readLastProject, rememberLastProject } from "@/lib/last-project";
import { STORAGE_PREFIX } from "@/product";

const KEY = `${STORAGE_PREFIX}settings.last_project.acme`;

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.restoreAllMocks();
});

it("remembers one project per organization", () => {
  rememberLastProject("acme", { slug: "support-bot", name: "Support bot" });
  rememberLastProject("acme", { slug: "playground", name: "Playground" });
  rememberLastProject("initech", { slug: "billing", name: "Billing" });

  expect(readLastProject("acme")).toEqual({ slug: "playground", name: "Playground" });
  expect(readLastProject("initech")).toEqual({ slug: "billing", name: "Billing" });
  expect(readLastProject("other")).toBeUndefined();
});

it("stores the slug and name under the prefixed key", () => {
  rememberLastProject("acme", { slug: "support-bot", name: "Support bot" });

  expect(JSON.parse(localStorage.getItem(KEY) ?? "null")).toEqual({
    slug: "support-bot",
    name: "Support bot",
  });
});

it("ignores stored values that are not a project", () => {
  for (const raw of [
    "not json",
    "null",
    "[]",
    '"support-bot"',
    '{"slug":"x"}',
    '{"slug":"","name":"A"}',
    '{"slug":1,"name":"A"}',
  ]) {
    localStorage.setItem(KEY, raw);
    expect(readLastProject("acme")).toBeUndefined();
  }
});

it("reads nothing and writes nothing when storage throws", () => {
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new Error("blocked");
  });
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("blocked");
  });

  expect(() => rememberLastProject("acme", { slug: "a", name: "A" })).not.toThrow();
  expect(readLastProject("acme")).toBeUndefined();
});
