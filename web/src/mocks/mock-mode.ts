import { STORAGE_PREFIX } from "@/product";

export const MOCK_STORAGE_KEY = `${STORAGE_PREFIX}mock`;

/**
 * The dev server serves the large seeded dataset by default (D131). `mock=fixtures` switches to the
 * small contract fixtures and remembers it for the tab's session; `mock=large` switches back.
 */
export function readMockMode(): "large" | "fixtures" {
  try {
    const requested = new URLSearchParams(window.location.search).get("mock");
    if (requested === "fixtures") {
      sessionStorage.setItem(MOCK_STORAGE_KEY, "fixtures");
      return "fixtures";
    }
    if (requested === "large") {
      sessionStorage.removeItem(MOCK_STORAGE_KEY);
      return "large";
    }
    return sessionStorage.getItem(MOCK_STORAGE_KEY) === "fixtures" ? "fixtures" : "large";
  } catch {
    return "large";
  }
}
