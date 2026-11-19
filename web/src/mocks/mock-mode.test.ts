import { afterEach, describe, expect, it } from "vitest";
import { MOCK_STORAGE_KEY, readMockMode } from "./mock-mode";

function visit(search: string) {
  window.history.replaceState({}, "", `/${search}`);
}

afterEach(() => {
  sessionStorage.clear();
  visit("");
});

describe("the dev server's mock data", () => {
  it("is the large dataset by default", () => {
    expect(readMockMode()).toBe("large");
  });

  it("switches to the fixtures with mock=fixtures, and remembers it for the session", () => {
    visit("?mock=fixtures");
    expect(readMockMode()).toBe("fixtures");
    visit("");
    expect(readMockMode()).toBe("fixtures");
    expect(sessionStorage.getItem(MOCK_STORAGE_KEY)).toBe("fixtures");
  });

  it("switches back to the large dataset with mock=large", () => {
    visit("?mock=fixtures");
    readMockMode();
    visit("?mock=large");
    expect(readMockMode()).toBe("large");
    visit("");
    expect(readMockMode()).toBe("large");
  });
});
