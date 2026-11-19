import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import meFixture from "../../../../../fixtures/api/static/me.json";

const ADMIN_ME = { ...meFixture, orgs: [{ ...meFixture.orgs[0], role: "admin" }] };

it("shows the organization settings as cards with live statuses", async () => {
  await renderApp("/acme/settings");

  const main = await screen.findByRole("main");
  expect(
    within(main).getByRole("heading", { level: 1, name: "Organization settings" }),
  ).toBeInTheDocument();
  expect(
    await within(main).findByRole("link", { name: /Plan, limits and usage/ }),
  ).toBeInTheDocument();
  expect(await within(main).findByText("Plan: Pro · 1% of units used")).toBeInTheDocument();
  expect(await within(main).findByText("2 members · 1 pending invite")).toBeInTheDocument();
  expect(await within(main).findByText("1 connection")).toBeInTheDocument();
  expect(await within(main).findByText("1 own price")).toBeInTheDocument();
  expect(within(main).getByText("Slug: acme")).toBeInTheDocument();
  expect(await within(main).findByText("Enabled")).toBeInTheDocument();
});

it("does not link the overview to itself", async () => {
  await renderApp("/acme/settings");
  const main = await screen.findByRole("main");
  await within(main).findByText("Slug: acme");
  expect(within(main).queryByRole("link", { name: /Every organization setting/ })).toBeNull();
});

it("hides the owner-only single sign-on card from admins", async () => {
  server.use(http.get("/api/auth/me", () => HttpResponse.json(ADMIN_ME)));

  await renderApp("/acme/settings");
  const main = await screen.findByRole("main");
  // The invite count only shows once the admin role has loaded, so the card check below is final.
  expect(await within(main).findByText("2 members · 1 pending invite")).toBeInTheDocument();
  expect(within(main).queryByRole("link", { name: /Single sign-on/ })).toBeNull();
  expect(within(main).getByRole("link", { name: /Members and invites/ })).toBeInTheDocument();
});
