import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  errorRate,
  filterProjects,
  getProjectView,
  PROJECT_VIEW_STORAGE_KEY,
  setProjectView,
  useProjectView,
} from "@/lib/org-home";

describe("org home view preference", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    localStorage.clear();
    setProjectView("cards");
  });

  it("defaults to cards", () => {
    localStorage.clear();
    expect(getProjectView()).toBe("cards");
  });

  it("stores the choice in localStorage", () => {
    setProjectView("table");
    expect(getProjectView()).toBe("table");
    expect(localStorage.getItem(PROJECT_VIEW_STORAGE_KEY)).toBe("table");
  });

  it("reads a garbage stored value as cards", () => {
    localStorage.setItem(PROJECT_VIEW_STORAGE_KEY, "nope");
    expect(getProjectView()).toBe("cards");
  });

  it("re-renders a hook consumer when the view changes", () => {
    const { result } = renderHook(() => useProjectView());
    expect(result.current).toBe("cards");

    act(() => {
      setProjectView("table");
    });

    expect(result.current).toBe("table");
  });

  it("keeps the choice when storage throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });

    expect(() => {
      setProjectView("table");
    }).not.toThrow();
    expect(getProjectView()).toBe("table");
  });
});

describe("filterProjects", () => {
  const projects = [
    { name: "Playground", slug: "play" },
    { name: "Support bot", slug: "support-bot" },
    { name: "Batch jobs", slug: "batch" },
  ];

  it("keeps all for a blank query", () => {
    expect(filterProjects(projects, "  ")).toEqual(projects);
  });

  it("matches name or slug ignoring case", () => {
    expect(filterProjects(projects, "PLAY")).toEqual([projects[0]]);
    expect(filterProjects(projects, "support-b")).toEqual([projects[1]]);
  });

  it("trims the query", () => {
    expect(filterProjects(projects, "  batch  ")).toEqual([projects[2]]);
  });

  it("keeps the input order", () => {
    expect(filterProjects(projects, "a")).toEqual([projects[0], projects[2]]);
  });
});

describe("errorRate", () => {
  it("is undefined without traces", () => {
    expect(errorRate(0, 0)).toBeUndefined();
  });

  it("is errors over traces", () => {
    expect(errorRate(42, 3)).toBeCloseTo(3 / 42);
  });
});
