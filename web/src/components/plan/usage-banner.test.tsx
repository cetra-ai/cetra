import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { orgEntitlementsQuery } from "@/api/queries-settings";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import entitlementsFixture from "../../../../fixtures/api/static/org-entitlements.json";

const ownerMe = {
  user: {
    id: "01900000-0000-7000-8000-000000000100",
    email: "ada@example.com",
    name: "Ada Example",
    is_instance_admin: true,
  },
  orgs: [
    {
      id: "01900000-0000-7000-8000-000000000010",
      slug: "acme",
      name: "Acme",
      role: "owner",
    },
  ],
};

/** Entitlements with this usage notice and `limits`, on top of the fixture. */
function entitlementsWith(
  notice: "none" | "warning" | "reached",
  limits: Record<string, number> = entitlementsFixture.limits,
) {
  return {
    ...entitlementsFixture,
    limits,
    usage: { ...entitlementsFixture.usage, notice, units: 170000 },
  };
}

it("warns at 80 % of the included units", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json(entitlementsWith("warning")),
    ),
  );

  await renderApp("/acme/support-bot/dashboard");

  expect(await screen.findByRole("status")).toHaveTextContent(
    "This organization has used 80% of its monthly units.",
  );
  expect(screen.getByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});

it("says ingest is paused until the reset when a cap is reached", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json(
        entitlementsWith("reached", {
          ...entitlementsFixture.limits,
          units_cap: 200000,
        }),
      ),
    ),
  );

  await renderApp("/acme/support-bot/dashboard");

  expect(await screen.findByRole("status")).toHaveTextContent(
    "This organization reached its monthly units; ingest is paused until 2026-02-01 (UTC).",
  );
});

it("says extra units are billed when the cap is above the included units", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json(
        entitlementsWith("reached", {
          ...entitlementsFixture.limits,
          units_cap: 300000,
        }),
      ),
    ),
  );

  await renderApp("/acme/support-bot/dashboard");

  expect(await screen.findByRole("status")).toHaveTextContent(
    "This organization used all units included in its plan; additional units are billed as overage.",
  );
});

it("says extra units are billed when no cap is set", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json(entitlementsWith("reached")),
    ),
  );

  await renderApp("/acme/support-bot/dashboard");

  expect(await screen.findByRole("status")).toHaveTextContent(
    "This organization used all units included in its plan; additional units are billed as overage.",
  );
});

it("is hidden for members", async () => {
  server.use(
    http.get("/api/auth/me", () =>
      HttpResponse.json({ ...ownerMe, orgs: [{ ...ownerMe.orgs[0], role: "member" }] }),
    ),
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json(entitlementsWith("warning")),
    ),
  );

  await renderApp("/acme/support-bot/dashboard");

  expect(await screen.findByRole("heading", { name: "Dashboard" })).toBeInTheDocument();
  expect(screen.queryByRole("status")).toBeNull();
});

it("is hidden when no notice applies", async () => {
  const { queryClient } = await renderApp("/acme/support-bot/dashboard");

  expect(await screen.findByRole("heading", { name: "Dashboard" })).toBeInTheDocument();
  await waitFor(() =>
    expect(queryClient.getQueryState(orgEntitlementsQuery("acme").queryKey)?.status).toBe(
      "success",
    ),
  );
  expect(screen.queryByRole("status")).toBeNull();
});
