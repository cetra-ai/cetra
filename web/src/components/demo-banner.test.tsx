import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { authConfigQuery } from "@/api/auth";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import authConfig from "../../../fixtures/api/static/auth-config.json";

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

const NOTICE = "You're exploring the cetra demo, read-only.";

function signedIn(orgs: unknown[]) {
  server.use(
    http.get("/api/auth/me", () => HttpResponse.json({ user: USER, orgs })),
    http.get("/api/v1/orgs/:org/projects", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
}

it("shows the read-only notice to a viewer, linking to their own org", async () => {
  signedIn([OWN, DEMO]);

  await renderApp("/cetra-demo");
  const notice = screen.getByRole("status", { name: "Demo" });
  expect(notice).toHaveTextContent(NOTICE);
  expect(screen.getByRole("link", { name: "Send your own traces" })).toHaveAttribute("href", "/");
});

it("hides the notice from an admin of the demo org", async () => {
  signedIn([OWN, { ...DEMO, role: "admin" }]);

  await renderApp("/cetra-demo");
  expect(screen.queryByRole("status", { name: "Demo" })).toBeNull();
});

it("hides the notice in an org that is not the demo", async () => {
  signedIn([OWN, DEMO]);

  await renderApp("/acme");
  expect(screen.queryByRole("status", { name: "Demo" })).toBeNull();
});

it("links a demo-only user to the create URL when one is set", async () => {
  signedIn([DEMO]);
  server.use(
    http.get("/api/auth/config", () =>
      HttpResponse.json({
        ...authConfig,
        org_create_allowed: false,
        org_create_url: "https://cloud.example.com/new",
      }),
    ),
  );

  await renderApp("/cetra-demo");
  expect(await screen.findByRole("link", { name: "Send your own traces" })).toHaveAttribute(
    "href",
    "https://cloud.example.com/new",
  );
});

it("links a demo-only user to workspace setup when they may create an org", async () => {
  signedIn([DEMO]);

  await renderApp("/cetra-demo");
  expect(await screen.findByRole("link", { name: "Send your own traces" })).toHaveAttribute(
    "href",
    "/setup/workspace",
  );
});

it("offers no link to a demo-only user who may not create an org and has no URL", async () => {
  signedIn([DEMO]);
  server.use(
    http.get("/api/auth/config", () =>
      HttpResponse.json({ ...authConfig, org_create_allowed: false }),
    ),
  );

  const { queryClient } = await renderApp("/cetra-demo");
  await waitFor(() =>
    expect(queryClient.getQueryData(authConfigQuery.queryKey)).toMatchObject({
      org_create_allowed: false,
    }),
  );
  expect(screen.getByRole("status", { name: "Demo" })).toHaveTextContent(NOTICE);
  expect(screen.queryByRole("link", { name: "Send your own traces" })).toBeNull();
});
