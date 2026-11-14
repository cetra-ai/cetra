import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import entitlementsFixture from "../../../../../fixtures/api/static/org-entitlements.json";

it("shows the plan, features, limits and upgrade link", async () => {
  await renderApp("/acme/settings/plan");

  expect(await screen.findByRole("heading", { name: "Plan", level: 1 })).toBeInTheDocument();
  const nav = within(screen.getByRole("list", { name: "Organization settings" }));
  expect(nav.getByRole("link", { name: "Plan" })).toHaveAttribute("aria-current", "page");

  expect(await screen.findByRole("heading", { name: "Pro + Teams" })).toBeInTheDocument();
  expect(screen.queryByText("Instance defaults apply.")).toBeNull();

  expect(screen.getByText("Project roles").closest("li")).toHaveTextContent("Included");
  expect(screen.getByText("Retention management").closest("li")).toHaveTextContent("Included");
  const sso = screen.getByText("SSO enforcement").closest("li");
  expect(sso).toHaveTextContent("Available with an enterprise licence");
  expect(sso).not.toHaveTextContent("Included");

  const limits = screen.getByRole("table");
  expect(
    within(limits).getByRole("row", { name: /Units per month \(included\)/ }),
  ).toHaveTextContent("200,000");
  expect(within(limits).getByRole("row", { name: /Monthly cap/ })).toHaveTextContent("unlimited");
  expect(within(limits).getByRole("row", { name: /Ingest rate/ })).toHaveTextContent("20,000");
  expect(within(limits).getByRole("row", { name: /Ingest burst/ })).toHaveTextContent("100,000");
  expect(within(limits).getByRole("row", { name: /Data access/ })).toHaveTextContent("1,825 days");
  expect(within(limits).getByRole("row", { name: /Seats/ })).toHaveTextContent("unlimited");

  expect(screen.getByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});

it("says instance defaults apply and lists unlimited limits", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({
        features: [],
        plan: { name: "Self-hosted", add_ons: [], source: "defaults" },
        limits: {},
      }),
    ),
  );

  await renderApp("/acme/settings/plan");

  expect(await screen.findByRole("heading", { name: "Self-hosted" })).toBeInTheDocument();
  expect(screen.getByText("Instance defaults apply.")).toBeInTheDocument();
  expect(screen.getAllByText("unlimited")).toHaveLength(9);
});

it("warns about the grace period", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({
        ...entitlementsFixture,
        plan: {
          ...entitlementsFixture.plan,
          status: "grace",
          expires_at: "2026-02-05T00:00:00.000000000Z",
          grace_ends_at: "2026-02-12T00:00:00.000000000Z",
        },
      }),
    ),
  );

  await renderApp("/acme/settings/plan");

  expect(
    await screen.findByText(
      "This plan expired on 2026-02-05. It stays in effect until 2026-02-12.",
    ),
  ).toBeInTheDocument();
});

it("has no upgrade link without an upgrade URL", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () => {
      const { upgrade_url: _omitted, ...rest } = entitlementsFixture;
      return HttpResponse.json(rest);
    }),
  );

  await renderApp("/acme/settings/plan");

  expect(await screen.findByRole("heading", { name: "Pro + Teams" })).toBeInTheDocument();
  expect(screen.queryByRole("link", { name: "Upgrade" })).toBeNull();
});
