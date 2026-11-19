import { expect, it } from "vitest";
import { parseSettingsListSearch } from "./settings-list";

const SORTS = ["name", "created"] as const;
const TABS = ["members", "invites"] as const;

it("keeps a trimmed query and drops an empty or too long one", () => {
  expect(parseSettingsListSearch({ q: "  bot  " }, SORTS)).toEqual({ q: "bot" });
  expect(parseSettingsListSearch({ q: "   " }, SORTS)).toEqual({});
  expect(parseSettingsListSearch({ q: "x".repeat(201) }, SORTS)).toEqual({});
  expect(parseSettingsListSearch({ q: "x".repeat(200) }, SORTS)).toEqual({ q: "x".repeat(200) });
});

it("keeps only allowed sorts", () => {
  expect(parseSettingsListSearch({ sort: "created" }, SORTS)).toEqual({ sort: "created" });
  expect(parseSettingsListSearch({ sort: "bogus" }, SORTS)).toEqual({});
});

it("reads desc from true or the string true only", () => {
  expect(parseSettingsListSearch({ desc: true }, SORTS)).toEqual({ desc: true });
  expect(parseSettingsListSearch({ desc: "true" }, SORTS)).toEqual({ desc: true });
  expect(parseSettingsListSearch({ desc: false }, SORTS)).toEqual({});
  expect(parseSettingsListSearch({ desc: "yes" }, SORTS)).toEqual({});
});

it("keeps an open id up to 256 characters", () => {
  expect(parseSettingsListSearch({ open: "key-1" }, SORTS)).toEqual({ open: "key-1" });
  expect(parseSettingsListSearch({ open: "x".repeat(257) }, SORTS)).toEqual({});
});

it("keeps only allowed tabs, and none when no tabs are given", () => {
  expect(parseSettingsListSearch({ tab: "invites" }, SORTS, TABS)).toEqual({ tab: "invites" });
  expect(parseSettingsListSearch({ tab: "other" }, SORTS, TABS)).toEqual({});
  expect(parseSettingsListSearch({ tab: "members" }, SORTS)).toEqual({});
});

it("omits every absent key", () => {
  expect(parseSettingsListSearch({}, SORTS, TABS)).toEqual({});
  expect(parseSettingsListSearch({ q: 3, sort: 4, open: null }, SORTS)).toEqual({});
});
