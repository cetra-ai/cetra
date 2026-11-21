import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import entitlementsFixture from "../../../../../fixtures/api/static/org-entitlements.json";
import usageFixture from "../../../../../fixtures/api/static/org-usage.json";

it("shows the month's units against the included units and the reset date", async () => {
  await renderApp("/acme/settings/plan");

  const page = within(await screen.findByRole("region", { name: "Usage this month" }));
  const meter = page.getByRole("meter", { name: "Units this month" });
  expect(meter).toHaveAttribute("aria-valuenow", "1240");
  expect(meter).toHaveAttribute("aria-valuemax", "200000");
  expect(page.getByText("1,240 of 200,000 units")).toBeInTheDocument();
  expect(page.getByText("Resets on 2026-02-01 (UTC).")).toBeInTheDocument();
  expect(page.queryByText(/Ingest pauses at/)).toBeNull();
  expect(await screen.findByRole("img", { name: "Daily units" })).toBeInTheDocument();
});

it("measures against the cap and says when ingest pauses without an included value", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () => {
      const { units_included: _omitted, ...limits } = entitlementsFixture.limits;
      return HttpResponse.json({
        ...entitlementsFixture,
        limits: { ...limits, units_cap: 250_000 },
      });
    }),
  );

  await renderApp("/acme/settings/plan");

  const page = within(await screen.findByRole("region", { name: "Usage this month" }));
  const meter = page.getByRole("meter", { name: "Units this month" });
  expect(meter).toHaveAttribute("aria-valuemax", "250000");
  expect(page.getByText("1,240 of 250,000 units")).toBeInTheDocument();
  expect(page.getByText("Ingest pauses at 250,000 units.")).toBeInTheDocument();
});

it("shows only the usage text when neither an included value nor a cap is set", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () => {
      const { units_included: _omitted, ...limits } = entitlementsFixture.limits;
      return HttpResponse.json({ ...entitlementsFixture, limits });
    }),
  );

  await renderApp("/acme/settings/plan");

  const page = within(await screen.findByRole("region", { name: "Usage this month" }));
  expect(page.getByText("1,240 units")).toBeInTheDocument();
  expect(page.queryByRole("meter")).toBeNull();
});

it("totals the daily usage", async () => {
  await renderApp("/acme/settings/plan");

  const totals = await screen.findByRole("table", { name: "Usage totals" });
  expect(within(totals).getByRole("cell", { name: "2,000" })).toBeInTheDocument();
  expect(within(totals).getByRole("cell", { name: "50" })).toBeInTheDocument();
  expect(within(totals).getByRole("cell", { name: "2,050" })).toBeInTheDocument();
  expect(within(totals).getByRole("cell", { name: "5.6 MB" })).toBeInTheDocument();
  expect(within(totals).getByRole("cell", { name: "$0.42" })).toBeInTheDocument();
});

it("says when there is no usage this month", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/usage", () => HttpResponse.json({ ...usageFixture, days: [] })),
  );

  await renderApp("/acme/settings/plan");

  expect(await screen.findByText("No usage this month.")).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Usage totals" })).toBeNull();
  expect(screen.queryByRole("img", { name: "Daily units" })).toBeNull();
});

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

  const limits = screen.getByRole("region", { name: "Limits" });
  expect(
    within(limits).getByRole("row", { name: /Units per month \(included\)/ }),
  ).toHaveTextContent("200,000");
  expect(within(limits).getByRole("row", { name: /Monthly cap/ })).toHaveTextContent("unlimited");
  expect(within(limits).getByRole("row", { name: /Ingest rate/ })).toHaveTextContent("20,000");
  expect(within(limits).getByRole("row", { name: /Ingest burst/ })).toHaveTextContent("100,000");
  expect(within(limits).getByRole("row", { name: /Data access/ })).toHaveTextContent("1,825 days");
  expect(within(limits).getByRole("row", { name: /Seats/ })).toHaveTextContent("unlimited");
  expect(within(limits).getByRole("row", { name: /Annotation queues/ })).toHaveTextContent(
    "unlimited",
  );

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
        usage: { ...entitlementsFixture.usage, units: 0, notice: "none" },
      }),
    ),
  );

  await renderApp("/acme/settings/plan");

  expect(await screen.findByRole("heading", { name: "Self-hosted" })).toBeInTheDocument();
  expect(screen.getByText("Instance defaults apply.")).toBeInTheDocument();
  expect(screen.getAllByText("unlimited")).toHaveLength(11);
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
