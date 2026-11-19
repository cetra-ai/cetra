import { expect, it } from "vitest";
import { NAV_SECTIONS, projectNav } from "@/components/shell/nav";
import { GO_TARGETS, goKeyOf } from "@/lib/shortcuts";

it("gives every sidebar entry a go letter", () => {
  for (const item of projectNav) {
    expect(goKeyOf(item.to), item.label).toBeDefined();
  }
});

it("gives each sidebar entry a different go letter", () => {
  const letters = projectNav.map((item) => goKeyOf(item.to));
  expect(new Set(letters).size).toBe(projectNav.length);
});

it("lists every go target in the sidebar", () => {
  const routes = projectNav.map((item) => item.to as string);
  for (const target of Object.values(GO_TARGETS)) {
    expect(routes).toContain(target.to);
  }
});

it("orders the sidebar by section, each section in one run", () => {
  const sections = projectNav.map((item) => item.section as string);
  const runs = sections.filter((section, index) => section !== sections[index - 1]);
  expect(runs).toEqual([...NAV_SECTIONS]);
});
