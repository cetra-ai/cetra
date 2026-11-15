import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import entitlementsFixture from "../../../../fixtures/api/static/org-entitlements.json";

const NOTE = "Data older than 7 days: available on a paid plan.";

/** Entitlements with `limits` replaced, on top of the fixture. */
function withLimits(limits: Record<string, number>) {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({ ...entitlementsFixture, limits }),
    ),
  );
}

async function openPicker(user: Awaited<ReturnType<typeof renderApp>>["user"]) {
  await user.click(await screen.findByRole("button", { name: "Last 24 hours" }));
  return screen.findByRole("dialog");
}

it("locks nothing when the fixture allows 1825 days", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  const dialog = await openPicker(user);

  expect(within(dialog).getByRole("button", { name: "Last 30 days" })).toBeEnabled();
  expect(within(dialog).queryByRole("note")).toBeNull();
});

it("locks presets beyond the window and links to the upgrade page", async () => {
  withLimits({ data_access_days: 7 });
  const { user } = await renderApp("/acme/support-bot/traces");
  const dialog = await openPicker(user);

  expect(
    within(dialog).getByRole("button", { name: "Last 30 days, available on a paid plan" }),
  ).toBeDisabled();
  expect(within(dialog).getByRole("button", { name: "Last 7 days" })).toBeEnabled();
  expect(within(dialog).getByRole("note")).toHaveTextContent(NOTE);
  expect(within(dialog).getByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});

it("keeps the popover open and shows the note for a typed range beyond the window", async () => {
  withLimits({ data_access_days: 7 });
  const { user, router } = await renderApp("/acme/support-bot/traces");
  const dialog = await openPicker(user);

  await user.type(within(dialog).getByLabelText("Duration"), "10d{Enter}");

  expect(screen.getByRole("dialog")).toBeInTheDocument();
  expect(within(dialog).getAllByText(NOTE)).toHaveLength(2);
  expect(router.state.location.search).not.toHaveProperty("from");
});

it("applies a typed range inside the window", async () => {
  withLimits({ data_access_days: 7 });
  const { user, router } = await renderApp("/acme/support-bot/traces");
  const dialog = await openPicker(user);

  await user.type(within(dialog).getByLabelText("Duration"), "3d{Enter}");

  expect(screen.queryByRole("dialog")).toBeNull();
  expect(router.state.location.search).toHaveProperty("from");
});

it("locks nothing when the limits omit the data window", async () => {
  withLimits({ units_included: 200000 });
  const { user } = await renderApp("/acme/support-bot/traces");
  const dialog = await openPicker(user);

  expect(within(dialog).getByRole("button", { name: "Last 30 days" })).toBeEnabled();
  expect(within(dialog).queryByRole("note")).toBeNull();
});
