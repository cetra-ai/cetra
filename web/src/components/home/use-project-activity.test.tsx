import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { type ReactNode, useState } from "react";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { activityCells, useProjectActivity } from "@/components/home/use-project-activity";
import { server } from "@/mocks/node";
import projectsFixture from "../../../../fixtures/api/static/projects.json";
import activityFixture from "../../../../fixtures/api/static/projects-activity.json";

const SUPPORT_ID = "01900000-0000-7000-8000-000000000001";
const PLAYGROUND_ID = "01900000-0000-7000-8000-000000000002";

const activity = activityFixture as Schema<"ProjectsActivity">;
const projects = projectsFixture.data as Schema<"Project">[];

function fixtureActivity(id: string): Schema<"ProjectActivity"> {
  const found = activity.data.find((row) => row.project_id === id);
  if (!found) {
    throw new Error(`no activity fixture for ${id}`);
  }
  return found;
}

/** One query client per mounted test, so the answers stay cached while it runs. */
function Wrapper({ children }: { children: ReactNode }) {
  const [client] = useState(
    () => new QueryClient({ defaultOptions: { queries: { retry: false } } }),
  );
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

describe("useProjectActivity", () => {
  it("reads ready activity from the fixture", async () => {
    const { result } = renderHook(() => useProjectActivity("acme", [projects]), {
      wrapper: Wrapper,
    });

    await waitFor(() => expect(result.current(SUPPORT_ID).state).toBe("ready"));
    const state = result.current(SUPPORT_ID);
    expect(state.state).toBe("ready");
    if (state.state === "ready") {
      expect(state.activity.traces).toBe(42);
      expect(state.from).toBe(activity.from);
    }
  });

  it("is loading before the answer", () => {
    const { result } = renderHook(() => useProjectActivity("acme", [projects]), {
      wrapper: Wrapper,
    });

    expect(result.current(SUPPORT_ID)).toEqual({ state: "loading" });
  });

  it("reads none for an unavailable project", async () => {
    server.use(
      http.get("/api/v1/orgs/:org/projects/activity", () => {
        const body: Schema<"ProjectsActivity"> = {
          ...activity,
          data: [],
          unavailable: [SUPPORT_ID],
        };
        return HttpResponse.json(body);
      }),
    );
    const { result } = renderHook(() => useProjectActivity("acme", [projects]), {
      wrapper: Wrapper,
    });

    await waitFor(() => expect(result.current(SUPPORT_ID).state).toBe("none"));
  });

  it("reads none when the request fails", async () => {
    server.use(
      http.get("/api/v1/orgs/:org/projects/activity", () =>
        HttpResponse.json(
          { type: "about:blank", title: "Internal error", status: 500, code: "internal" },
          { status: 500 },
        ),
      ),
    );
    const { result } = renderHook(() => useProjectActivity("acme", [projects]), {
      wrapper: Wrapper,
    });

    await waitFor(() => expect(result.current(SUPPORT_ID).state).toBe("none"));
  });

  it("asks once per non-empty page with that page's ids", async () => {
    const requests: string[] = [];
    server.use(
      http.get("/api/v1/orgs/:org/projects/activity", ({ request }) => {
        requests.push(new URL(request.url).searchParams.get("ids") ?? "");
        return HttpResponse.json(activity);
      }),
    );
    const { result } = renderHook(
      () =>
        useProjectActivity("acme", [
          [projects[0] as Schema<"Project">],
          [],
          [projects[1] as Schema<"Project">],
        ]),
      { wrapper: Wrapper },
    );

    await waitFor(() => expect(result.current(PLAYGROUND_ID).state).toBe("ready"));
    // The two pages load in parallel, so the order of the requests is not fixed.
    expect([...requests].sort()).toEqual([SUPPORT_ID, PLAYGROUND_ID]);
  });
});

describe("activityCells", () => {
  it("is undefined while loading", () => {
    expect(activityCells({ state: "loading" })).toBeUndefined();
  });

  it("shows dashes when the activity is none", () => {
    expect(activityCells({ state: "none" })).toEqual({
      traces: "—",
      errorRate: "—",
      cost: "—",
    });
  });

  it("formats a project with traces", () => {
    const support = fixtureActivity(SUPPORT_ID);
    expect(activityCells({ state: "ready", from: activity.from, activity: support })).toEqual({
      traces: "42",
      errorRate: "7.1%",
      cost: "$1.23",
      lastIngestAt: support.last_ingest_at,
    });
  });

  it("formats a quiet project without a last trace", () => {
    const playground = fixtureActivity(PLAYGROUND_ID);
    const cells = activityCells({ state: "ready", from: activity.from, activity: playground });
    expect(cells).toEqual({ traces: "0", errorRate: "—", cost: "$0" });
    expect(cells?.lastIngestAt).toBeUndefined();
  });
});
