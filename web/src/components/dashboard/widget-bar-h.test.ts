import { expect, it } from "vitest";
import { labelWidth } from "@/components/dashboard/widget-bar-h";

it("sizes the label column to the longest group name", () => {
  expect(labelWidth(["github.com/acme/web", "github.com/acme/infra"])).toBe(
    Math.ceil("github.com/acme/infra".length * 7) + 12,
  );
});

it("keeps short labels at the minimum and caps long ones", () => {
  expect(labelWidth(["a", "b"])).toBe(48);
  expect(labelWidth([])).toBe(48);
  expect(labelWidth(["x".repeat(200)])).toBe(240);
});
