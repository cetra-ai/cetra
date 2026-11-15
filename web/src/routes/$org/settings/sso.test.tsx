import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import meFixture from "../../../../../fixtures/api/static/me.json";
import entitlementsFixture from "../../../../../fixtures/api/static/org-entitlements.json";
import ssoFixture from "../../../../../fixtures/api/static/org-sso.json";

const ACME_DOMAIN_ID = "01900000-0000-7000-8000-000000000701";
const orgNav = () => within(screen.getByRole("list", { name: "Organization settings" }));

function meAs(role: string) {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: meFixture.user,
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role }],
    }),
  );
}

/** Renders the page as `role`; `locked` drops `org_sso` from the org's features. */
async function openPage({ role = "owner", locked = false } = {}) {
  const features = locked
    ? entitlementsFixture.features
    : [...entitlementsFixture.features, "org_sso"];
  server.use(
    meAs(role),
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({ ...entitlementsFixture, features }),
    ),
  );
  const rendered = await renderApp("/acme/settings/sso");
  return rendered;
}

it("shows the provider, the callback URL, the domains and the switches to an owner", async () => {
  const { router } = await openPage();

  await screen.findByRole("heading", { name: "Identity provider", level: 2 });
  expect(router.state.location.pathname).toBe("/acme/settings/sso");
  expect(orgNav().getByRole("link", { name: "Single sign-on" })).toHaveAttribute(
    "aria-current",
    "page",
  );
  expect(screen.getByLabelText("Display name")).toHaveValue("Acme Okta");
  expect(screen.getByLabelText("Issuer URL")).toHaveValue("https://acme.okta.com");
  expect(screen.getByLabelText("Client ID")).toHaveValue("0oa1b2c3d4");
  expect(screen.getByLabelText("Client secret")).toHaveValue("");
  expect(screen.getByLabelText("Client secret")).toHaveAttribute(
    "placeholder",
    "Stored; leave blank to keep it",
  );
  expect(screen.getByText(ssoFixture.callback_url)).toBeInTheDocument();

  const table = screen.getByRole("table", { name: "Domains" });
  const rows = within(table).getAllByRole("row");
  expect(rows).toHaveLength(3);
  expect(within(table).getByText("acme.com")).toBeInTheDocument();
  expect(within(table).getByText("Verified")).toBeInTheDocument();
  expect(within(table).getByText("acme.io")).toBeInTheDocument();
  expect(within(table).getByText("Pending")).toBeInTheDocument();
  expect(within(table).getByText("_llmobs-verify.acme.io")).toBeInTheDocument();
  expect(
    within(table).getByText("llmobs-verify=fedcba9876543210fedcba9876543210"),
  ).toBeInTheDocument();

  expect(screen.getByRole("switch", { name: "Enabled" })).toBeChecked();
  expect(screen.getByRole("switch", { name: "Enforced" })).not.toBeChecked();
  expect(screen.getByRole("link", { name: "Test sign-in" })).toHaveAttribute(
    "href",
    "/api/auth/sso/authorize?domain=acme.com&redirect=%2Facme%2Fsettings%2Fsso",
  );
});

it("sends the secret only when one is typed", async () => {
  const bodies: unknown[] = [];
  server.use(
    http.put("/api/v1/orgs/:org/sso/provider", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json(ssoFixture);
    }),
  );

  const { user } = await openPage();
  await user.click(await screen.findByRole("button", { name: "Save" }));
  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  expect(bodies[0]).toEqual({
    display_name: "Acme Okta",
    issuer: "https://acme.okta.com",
    client_id: "0oa1b2c3d4",
    jit_role: "viewer",
  });

  await user.type(screen.getByLabelText("Client secret"), "s3cret");
  await user.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => {
    expect(bodies).toHaveLength(2);
  });
  expect(bodies[1]).toEqual({
    display_name: "Acme Okta",
    issuer: "https://acme.okta.com",
    client_id: "0oa1b2c3d4",
    jit_role: "viewer",
    client_secret: "s3cret",
  });
  expect(await screen.findByRole("status")).toHaveTextContent("Saved");
});

it("shows a field error under its field", async () => {
  server.use(
    http.put("/api/v1/orgs/:org/sso/provider", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "issuer", message: "issuer must be an https URL" }],
      }),
    ),
  );

  const { user } = await openPage();
  await user.click(await screen.findByRole("button", { name: "Save" }));

  expect(await screen.findByText("issuer must be an https URL")).toBeInTheDocument();
  expect(screen.getByLabelText("Issuer URL")).toHaveAttribute("aria-invalid", "true");
});

it("adds, verifies and removes domains", async () => {
  let added: unknown;
  let verifyFailed = false;
  let removedId: string | undefined;
  server.use(
    http.post("/api/v1/orgs/:org/sso/domains", async ({ request }) => {
      added = await request.json();
      return HttpResponse.json(ssoFixture, { status: 201 });
    }),
    http.post("/api/v1/orgs/:org/sso/domains/:id/verify", () => {
      verifyFailed = true;
      return problemResponse(409, "conflict", {
        detail: "The TXT record is not published yet.",
      });
    }),
    http.delete("/api/v1/orgs/:org/sso/domains/:id", ({ params }) => {
      removedId = String(params.id);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await openPage();
  await user.type(await screen.findByLabelText("Domain"), "example.org");
  await user.click(screen.getByRole("button", { name: "Add domain" }));
  await waitFor(() => {
    expect(added).toEqual({ domain: "example.org" });
  });

  const table = screen.getByRole("table", { name: "Domains" });
  await user.click(within(table).getByRole("button", { name: "Verify" }));
  await waitFor(() => {
    expect(verifyFailed).toBe(true);
  });
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "The TXT record is not published yet.",
  );

  const removeButtons = within(table).getAllByRole("button", { name: "Remove" });
  await user.click(removeButtons[0] as HTMLElement);
  const dialog = await screen.findByRole("alertdialog");
  expect(
    within(dialog).getByText(/no longer be sent to your identity provider/),
  ).toBeInTheDocument();
  expect(removedId).toBeUndefined();
  await user.click(within(dialog).getByRole("button", { name: "Remove domain" }));
  await waitFor(() => {
    expect(removedId).toBe(ACME_DOMAIN_ID);
  });
});

it("lists the verified domains before enforcing and shows a refusal", async () => {
  let patched: unknown;
  server.use(
    http.patch("/api/v1/orgs/:org/sso", async ({ request }) => {
      patched = await request.json();
      return problemResponse(409, "conflict", {
        detail: "Sign in through the provider once before you enforce it.",
      });
    }),
  );

  const { user } = await openPage();
  await user.click(await screen.findByRole("switch", { name: "Enforced" }));

  const dialog = await screen.findByRole("alertdialog");
  expect(within(dialog).getByText(/acme\.com/)).toBeInTheDocument();
  expect(patched).toBeUndefined();

  await user.click(within(dialog).getByRole("button", { name: "Enforce" }));
  await waitFor(() => {
    expect(patched).toEqual({ enforced: true });
  });
  expect(await within(dialog).findByRole("alert")).toHaveTextContent(
    "Sign in through the provider once before you enforce it.",
  );
});

it("locks the changes without the feature and keeps removal", async () => {
  await openPage({ locked: true });

  const note = await screen.findByRole("note", { name: "Locked feature" });
  expect(note).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Add domain" })).toBeDisabled();
  const table = screen.getByRole("table", { name: "Domains" });
  expect(within(table).getByRole("button", { name: "Verify" })).toBeDisabled();
  expect(within(table).getAllByRole("button", { name: "Remove" })[0]).toBeEnabled();
  expect(screen.getByRole("button", { name: "Remove provider" })).toBeEnabled();
  expect(screen.getByRole("switch", { name: "Enforced" })).toBeDisabled();
});

it("tells an admin that only owners can manage single sign-on", async () => {
  const { router } = await openPage({ role: "admin" });

  expect(await screen.findByText("Only owners can manage single sign-on.")).toBeInTheDocument();
  expect(router.state.location.pathname).toBe("/acme/settings/sso");
  expect(screen.queryByRole("switch", { name: "Enabled" })).not.toBeInTheDocument();
  expect(orgNav().queryByRole("link", { name: "Single sign-on" })).not.toBeInTheDocument();
});
