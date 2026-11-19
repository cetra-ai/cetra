import { screen } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { STORAGE_PREFIX } from "@/product";
import { renderApp } from "@/test/render";

const SIDEBAR_KEY = `${STORAGE_PREFIX}sidebar`;

it("redirects a project to traces and marks Traces active", async () => {
  const { router } = await renderApp("/acme/support-bot");

  expect(router.state.location.pathname).toBe("/acme/support-bot/traces");
  expect(screen.getByRole("link", { name: "Traces" })).toHaveAttribute("aria-current", "page");
  expect(screen.getByRole("heading", { name: "Traces" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Alerts" })).toBeInTheDocument();
  expect(await screen.findByRole("grid", { name: "Traces" })).toHaveTextContent(
    "chat claude-sonnet-4-5",
  );
  expect(screen.getByText("2.00s")).toBeInTheDocument();
});

it("shows not found when the project does not exist", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json(
        { type: "about:blank", title: "Not found", status: 404, code: "not_found" },
        { status: 404, headers: { "Content-Type": "application/problem+json" } },
      ),
    ),
  );

  await renderApp("/acme/missing");
  expect(screen.getByRole("heading", { name: "Page not found" })).toBeInTheDocument();
});

it("lists projects in the switcher and navigates to the chosen one", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");

  await user.click(await screen.findByRole("button", { name: "Support bot" }));
  expect(screen.getByRole("menuitemradio", { name: "Playground" })).toBeInTheDocument();
  await user.click(screen.getByRole("menuitemradio", { name: "Playground" }));
  expect(router.state.location.pathname).toBe("/acme/playground/traces");
});

it("loads another page of projects from the switcher", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/projects", ({ request }) => {
      const url = new URL(request.url);
      if (url.searchParams.get("cursor") === "c2") {
        return HttpResponse.json({
          data: [project("Second", "second")],
          next_cursor: null,
        });
      }
      return HttpResponse.json({
        data: [project("Support bot", "support-bot")],
        next_cursor: "c2",
      });
    }),
  );

  const { router, user } = await renderApp("/acme/support-bot/traces");
  await user.click(await screen.findByRole("button", { name: "Support bot" }));
  await user.click(screen.getByRole("menuitem", { name: "Load more" }));
  await user.click(await screen.findByRole("menuitemradio", { name: "Second" }));
  expect(router.state.location.pathname).toBe("/acme/second/traces");
});

it("persists sidebar collapse in localStorage across a re-render", async () => {
  const first = await renderApp("/acme/support-bot/traces");
  expect(localStorage.getItem(SIDEBAR_KEY)).toBeNull();

  await first.user.keyboard("{Control>}b{/Control}");
  expect(localStorage.getItem(SIDEBAR_KEY)).toBe("closed");
  expect(document.querySelector("[data-slot='sidebar']")).toHaveAttribute(
    "data-state",
    "collapsed",
  );
  expect(document.cookie).not.toContain("sidebar_state");

  first.unmount();
  await renderApp("/acme/support-bot/traces");
  expect(document.querySelector("[data-slot='sidebar']")).toHaveAttribute(
    "data-state",
    "collapsed",
  );
});

it("shows the demo notice on the pages of a project in the demo org", async () => {
  server.use(
    http.get("/api/auth/me", () =>
      HttpResponse.json({
        user: {
          id: "01900000-0000-7000-8000-000000000100",
          email: "ada@example.com",
          name: "Ada Example",
          is_instance_admin: false,
        },
        orgs: [
          {
            id: "01900000-0000-7000-8000-000000000020",
            slug: "cetra-demo",
            name: "cetra Demo",
            role: "viewer",
            demo: true,
          },
        ],
      }),
    ),
  );

  await renderApp("/cetra-demo/support-bot/traces");
  expect(screen.getByRole("status", { name: "Demo" })).toHaveTextContent(
    "You're exploring the cetra demo, read-only.",
  );
});

function project(name: string, slug: string) {
  return {
    id: `01900000-0000-7000-8000-${slug.padEnd(12, "0").slice(0, 12)}`,
    org_id: "01900000-0000-7000-8000-000000000010",
    slug,
    name,
    role: "owner",
    created_at: "2026-01-01T09:00:00.000000000Z",
  };
}

function projectWithDroppedLogs(count: number) {
  return {
    id: "01900000-0000-7000-8000-000000000001",
    org_id: "01900000-0000-7000-8000-000000000010",
    slug: "support-bot",
    name: "Support bot",
    trace_completion_timeout_s: 60,
    role: "owner",
    genai_log_events: count,
    created_at: "2026-01-01T09:00:00.000000000Z",
  };
}

it("says once that GenAI content sent as log events was dropped", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () => HttpResponse.json(projectWithDroppedLogs(1200))),
  );

  const first = await renderApp("/acme/support-bot/traces");
  const hint = await screen.findByRole("status");
  expect(hint).toHaveTextContent("GenAI content was sent as log events");
  expect(hint).toHaveTextContent("1,200 GenAI log records");

  await first.user.click(screen.getByRole("button", { name: "Dismiss" }));
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
  first.unmount();

  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByRole("grid", { name: "Traces" })).toBeInTheDocument();
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("shows no hint when no log event was dropped", async () => {
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByRole("grid", { name: "Traces" })).toBeInTheDocument();
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});
