import { screen } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const USER = {
  id: "01900000-0000-7000-8000-000000000100",
  email: "ada@example.com",
  name: "Ada Example",
  is_instance_admin: false,
};

const OWN = {
  id: "01900000-0000-7000-8000-000000000010",
  slug: "acme",
  name: "Acme",
  role: "owner",
  demo: false,
};

const DEMO = {
  id: "01900000-0000-7000-8000-000000000020",
  slug: "cetra-demo",
  name: "cetra Demo",
  role: "viewer",
  demo: true,
};

it("opens the demo org's first project", async () => {
  server.use(
    http.get("/api/auth/me", () => HttpResponse.json({ user: USER, orgs: [OWN, DEMO] })),
    http.get("/api/v1/orgs/:org/projects", () =>
      HttpResponse.json({
        data: [project("Playground", "playground"), project("Support bot", "support-bot")],
        next_cursor: null,
      }),
    ),
  );

  const { router } = await renderApp("/demo");
  expect(router.state.location.pathname).toBe("/cetra-demo/playground/traces");
});

it("opens the demo org's page when it has no project", async () => {
  server.use(
    http.get("/api/auth/me", () => HttpResponse.json({ user: USER, orgs: [OWN, DEMO] })),
    http.get("/api/v1/orgs/:org/projects", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  const { router } = await renderApp("/demo");
  expect(router.state.location.pathname).toBe("/cetra-demo");
  expect(await screen.findByText("No projects yet")).toBeInTheDocument();
});

function project(name: string, slug: string) {
  return {
    id: `01900000-0000-7000-8000-${slug.padEnd(12, "0").slice(0, 12)}`,
    org_id: DEMO.id,
    slug,
    name,
    role: "viewer",
    created_at: "2026-01-01T09:00:00.000000000Z",
  };
}

it("goes home when no org is the demo org", async () => {
  server.use(http.get("/api/auth/me", () => HttpResponse.json({ user: USER, orgs: [OWN] })));

  const { router } = await renderApp("/demo");
  expect(router.state.location.pathname).toBe("/acme");
});

it("sends a signed-out visitor to sign in first", async () => {
  server.use(
    http.get("/api/auth/me", () =>
      HttpResponse.json(
        { type: "about:blank", title: "Unauthorized", status: 401, code: "unauthorized" },
        { status: 401 },
      ),
    ),
  );

  const { router } = await renderApp("/demo");
  expect(router.state.location.pathname).toBe("/login");
  expect(router.state.location.search).toEqual({ redirect: "/demo" });
});
