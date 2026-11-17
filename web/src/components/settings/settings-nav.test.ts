import { expect, it } from "vitest";
import {
  isProjectSettingsPath,
  orgSettingsNav,
  projectSettingsNav,
} from "@/components/settings/settings-nav";

it("matches the settings area and every path below it", () => {
  expect(isProjectSettingsPath("/acme/bot/settings", "acme", "bot")).toBe(true);
  expect(isProjectSettingsPath("/acme/bot/settings/general", "acme", "bot")).toBe(true);
});

it("does not match other paths", () => {
  expect(isProjectSettingsPath("/acme/bot/settingsx", "acme", "bot")).toBe(false);
  expect(isProjectSettingsPath("/acme/bot/traces", "acme", "bot")).toBe(false);
  expect(isProjectSettingsPath("/acme/other/settings", "acme", "bot")).toBe(false);
});

it("starts both lists with an overview that every role sees", () => {
  expect(orgSettingsNav[0]).toMatchObject({
    label: "Overview",
    to: "/$org/settings",
    ownerOnly: false,
  });
  expect(projectSettingsNav[0]).toMatchObject({
    label: "Overview",
    to: "/$org/$project/settings",
    adminOnly: false,
  });
});

it("gives every item an icon, a one-line description and keywords", () => {
  for (const item of [...orgSettingsNav, ...projectSettingsNav]) {
    expect(item.icon).toBeTruthy();
    expect(item.description.length).toBeGreaterThan(0);
    expect(item.description).not.toContain("\n");
    expect(item.keywords.length).toBeGreaterThan(0);
  }
});

it("keeps the role flags on the items that had them", () => {
  const sso = orgSettingsNav.find((item) => item.label === "Single sign-on");
  expect(sso?.ownerOnly).toBe(true);
  const keys = projectSettingsNav.find((item) => item.label === "API keys");
  expect(keys?.adminOnly).toBe(true);
});
