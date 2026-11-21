import { afterEach, expect, it, vi } from "vitest";
import { replaceExternal, setExternalNavigator } from "@/lib/external-navigation";

afterEach(() => {
  vi.restoreAllMocks();
});

it("replaceExternal goes through the navigator's replace", () => {
  const navigated: string[] = [];
  const replaced: string[] = [];
  setExternalNavigator({
    navigate: (url) => {
      navigated.push(url);
    },
    replace: (url) => {
      replaced.push(url);
    },
  });

  replaceExternal("/api/auth/authorize/github");

  setExternalNavigator(undefined);
  expect(replaced).toEqual(["/api/auth/authorize/github"]);
  expect(navigated).toEqual([]);
});
