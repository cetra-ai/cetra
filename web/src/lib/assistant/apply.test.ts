import { describe, expect, it } from "vitest";
import { filterTarget, sqlTabDraft } from "@/lib/assistant/apply";
import type { FilterDraft } from "@/lib/assistant/types";

const base: FilterDraft = { view: "traces", include_eval: false, include_experiments: false };

describe("filterTarget", () => {
  it("opens the list of each view", () => {
    expect(filterTarget({ ...base, view: "traces" }).to).toBe("/$org/$project/traces");
    expect(filterTarget({ ...base, view: "spans" }).to).toBe("/$org/$project/spans");
    expect(filterTarget({ ...base, view: "sessions" }).to).toBe("/$org/$project/sessions");
    expect(filterTarget({ ...base, view: "users" }).to).toBe("/$org/$project/users");
  });

  it("keeps the filter and the search on traces and spans", () => {
    expect(
      filterTarget({ ...base, filter: "status = 'error'", search: "timeout", range: "24h" }).search,
    ).toEqual({ filter: "status = 'error'", search: "timeout", range: "24h" });
    expect(filterTarget({ ...base, view: "spans", search: "timeout" }).search).toEqual({
      search: "timeout",
    });
  });

  it("drops the search on sessions and users", () => {
    expect(filterTarget({ ...base, view: "sessions", search: "x" }).search).toEqual({});
    expect(filterTarget({ ...base, view: "users", search: "x", filter: "a = 1" }).search).toEqual({
      filter: "a = 1",
    });
  });

  it("converts epoch milliseconds to RFC 3339", () => {
    expect(filterTarget({ ...base, from: "1767225600000", to: "1767229200000" }).search).toEqual({
      from: "2026-01-01T00:00:00.000Z",
      to: "2026-01-01T01:00:00.000Z",
    });
  });

  it("keeps a range value that is not epoch milliseconds as given", () => {
    expect(
      filterTarget({ ...base, from: "2026-01-01T00:00:00Z", to: "2026-01-02T00:00:00Z" }).search,
    ).toEqual({
      from: "2026-01-01T00:00:00Z",
      to: "2026-01-02T00:00:00Z",
    });
  });

  it("keeps an unreadable epoch value as given instead of throwing", () => {
    const huge = "9".repeat(20);
    expect(filterTarget({ ...base, from: huge, to: "1767229200000" }).search).toEqual({
      from: huge,
      to: "2026-01-01T01:00:00.000Z",
    });
  });

  it("sets the flags", () => {
    expect(filterTarget({ ...base, include_eval: true, include_experiments: true }).search).toEqual(
      { eval: 1, exp: 1 },
    );
    expect(filterTarget(base).search).toEqual({});
  });
});

describe("sqlTabDraft", () => {
  it("stringifies non-string params and keeps string ones", () => {
    expect(
      sqlTabDraft({
        query: "SELECT * FROM traces WHERE n > {n} AND name = {name}",
        params: { n: 5, name: "chat" },
        columns: [],
      }),
    ).toEqual({
      query: "SELECT * FROM traces WHERE n > {n} AND name = {name}",
      params: { n: "5", name: "chat" },
    });
  });
});
