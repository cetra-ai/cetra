import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import { server } from "@/mocks/node";
import { ORG_ID } from "@/test/fixtures";
import { renderApp } from "@/test/render";
import meFixture from "../../../../../fixtures/api/static/me.json";
import entitlementsFixture from "../../../../../fixtures/api/static/org-entitlements.json";
import scimFixture from "../../../../../fixtures/api/static/org-scim.json";
import scimGroupsFixture from "../../../../../fixtures/api/static/org-scim-groups.json";
import scimGroupFixture from "../../../../../fixtures/api/static/scim-group.json";
import scimTokenCreated from "../../../../../fixtures/api/static/scim-token-created.json";

const NEEDS_DOMAIN = "Verify a domain on the Single sign-on page first";

function meAs(role: string) {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: meFixture.user,
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role }],
    }),
  );
}

/** Renders the page as `role`; `locked` drops `scim` from the org's features. `scim` overrides the settings. */
async function openPage({
  role = "owner",
  locked = false,
  scim = {},
}: {
  role?: string;
  locked?: boolean;
  scim?: Record<string, unknown>;
} = {}) {
  const features = locked
    ? entitlementsFixture.features
    : [...entitlementsFixture.features, "scim"];
  server.use(
    meAs(role),
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({ ...entitlementsFixture, features }),
    ),
    http.get("/api/v1/orgs/:org/scim", () => HttpResponse.json({ ...scimFixture, ...scim })),
  );
  return renderApp("/acme/settings/provisioning");
}

it("shows the locked state without the feature", async () => {
  await openPage({ locked: true });

  const note = await screen.findByRole("note", { name: "Locked feature" });
  expect(note).toHaveTextContent("SCIM provisioning is part of the Enterprise plan");
  expect(screen.queryByRole("switch", { name: "Enabled" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Create token" })).not.toBeInTheDocument();
});

it("owner enables SCIM and copies the base URL", async () => {
  let patched: unknown;
  server.use(
    http.patch("/api/v1/orgs/:org/scim", async ({ request }) => {
      patched = await request.json();
      return HttpResponse.json({ ...scimFixture, enabled: true });
    }),
  );

  const { user } = await openPage({ scim: { enabled: false } });
  // userEvent.setup() installs its own clipboard stub, so the stub goes in after it.
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  await user.click(await screen.findByRole("switch", { name: "Enabled" }));

  await waitFor(() => {
    expect(patched).toEqual({ enabled: true });
  });
  await user.click(screen.getByRole("button", { name: "Copy base URL" }));
  await waitFor(() => {
    expect(writeText).toHaveBeenCalledWith(scimFixture.base_url);
  });
});

it("switch disabled without a verified domain", async () => {
  await openPage({ scim: { enabled: false, verified_domains: 0 } });

  expect(await screen.findByRole("switch", { name: "Enabled" })).toBeDisabled();
  expect(screen.getAllByText(NEEDS_DOMAIN).length).toBeGreaterThan(0);
});

it("creates a token and shows it once", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/scim/tokens", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(scimTokenCreated, { status: 201 });
    }),
  );

  const { user } = await openPage();
  await user.type(await screen.findByLabelText("Token name"), "Entra ID");
  await user.click(screen.getByRole("button", { name: "Create token" }));

  await waitFor(() => {
    expect(body).toEqual({ name: "Entra ID" });
  });
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByLabelText("Secret")).toHaveValue(scimTokenCreated.secret);
  expect(within(dialog).getByText(/You won't see it again/)).toBeInTheDocument();

  await user.click(within(dialog).getByRole("button", { name: "Done" }));
  await waitFor(() => {
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
  expect(screen.queryByDisplayValue(scimTokenCreated.secret)).not.toBeInTheDocument();
});

it("create disabled at two tokens", async () => {
  const [token] = scimFixture.tokens;
  await openPage({
    scim: {
      tokens: [token, { ...token, id: "01900000-0000-7000-8000-000000000803", name: "Entra ID" }],
    },
  });

  await screen.findByRole("table", { name: "SCIM tokens" });
  expect(screen.getByRole("button", { name: "Create token" })).toBeDisabled();
  expect(
    screen.getAllByText("Two tokens are live. Revoke one to create another.").length,
  ).toBeGreaterThan(0);
});

it("revokes a token after confirmation", async () => {
  let revokedId: string | undefined;
  server.use(
    http.delete("/api/v1/orgs/:org/scim/tokens/:id", ({ params }) => {
      revokedId = String(params.id);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await openPage();
  const table = await screen.findByRole("table", { name: "SCIM tokens" });
  await user.click(within(table).getByRole("button", { name: "Revoke" }));

  const dialog = await screen.findByRole("alertdialog");
  const confirm = within(dialog).getByRole("button", { name: "Revoke" });
  expect(confirm).toBeDisabled();
  expect(revokedId).toBeUndefined();
  await user.type(within(dialog).getByRole("textbox"), "revoke");
  await user.click(confirm);

  await waitFor(() => {
    expect(revokedId).toBe(scimFixture.tokens[0]?.id);
  });
});

it("non-owner sees the note", async () => {
  await openPage({ role: "admin" });

  expect(await screen.findByText("Only owners can manage provisioning.")).toBeInTheDocument();
  expect(screen.queryByRole("switch", { name: "Enabled" })).not.toBeInTheDocument();
});

it("lists groups with their mappings", async () => {
  await openPage();

  const table = await screen.findByRole("table", { name: "SCIM groups" });
  const engineering = within(table).getByRole("row", { name: /Engineering/ });
  expect(engineering).toHaveTextContent("12");
  expect(engineering).toHaveTextContent("Member");
  await waitFor(() => {
    expect(engineering).toHaveTextContent("Support bot (Admin)");
  });
  const auditors = within(table).getByRole("row", { name: /Auditors/ });
  expect(auditors).toHaveTextContent("Not mapped");
  expect(auditors).toHaveTextContent("None");
});

it("maps a group to member and a project role", async () => {
  let put: { id: string; body: unknown } | undefined;
  server.use(
    http.put("/api/v1/orgs/:org/scim/groups/:id/mapping", async ({ request, params }) => {
      put = { id: String(params.id), body: await request.json() };
      return HttpResponse.json(scimGroupFixture);
    }),
  );

  const { user } = await openPage();
  const table = await screen.findByRole("table", { name: "SCIM groups" });
  await user.click(within(table).getByRole("button", { name: "Edit Auditors" }));

  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("combobox", { name: "Org role" }));
  await user.click(await screen.findByRole("option", { name: "Member" }));
  await user.click(within(dialog).getByRole("button", { name: "Add project" }));
  await user.click(within(dialog).getByRole("combobox", { name: "Project 1" }));
  await user.click(await screen.findByRole("option", { name: "Support bot" }));
  await user.click(within(dialog).getByRole("combobox", { name: "Role in project 1" }));
  await user.click(await screen.findByRole("option", { name: "Admin" }));
  await user.click(within(dialog).getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(put).toEqual({
      id: scimGroupsFixture.groups[1]?.id,
      body: {
        org_role: "member",
        projects: [{ project_id: "01900000-0000-7000-8000-000000000001", role: "admin" }],
      },
    });
  });
  await waitFor(() => {
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});

it("removes a mapping", async () => {
  let deletedId: string | undefined;
  server.use(
    http.delete("/api/v1/orgs/:org/scim/groups/:id/mapping", ({ params }) => {
      deletedId = String(params.id);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await openPage();
  const table = await screen.findByRole("table", { name: "SCIM groups" });
  await user.click(within(table).getByRole("button", { name: "Edit Engineering" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("button", { name: "Remove mapping" }));

  await waitFor(() => {
    expect(deletedId).toBe(scimGroupsFixture.groups[0]?.id);
  });
});

it("empty state", async () => {
  server.use(http.get("/api/v1/orgs/:org/scim/groups", () => HttpResponse.json({ groups: [] })));
  await openPage();

  expect(
    await screen.findByText("Your identity provider has not pushed any group yet."),
  ).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "SCIM groups" })).not.toBeInTheDocument();
});

it("lapsed org can still turn SCIM off and revoke tokens", async () => {
  let patched: unknown;
  let revokedId: string | undefined;
  server.use(
    http.patch("/api/v1/orgs/:org/scim", async ({ request }) => {
      patched = await request.json();
      return HttpResponse.json({ ...scimFixture, enabled: false });
    }),
    http.delete("/api/v1/orgs/:org/scim/tokens/:id", ({ params }) => {
      revokedId = String(params.id);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await openPage({ locked: true });
  expect(await screen.findByRole("note", { name: "Locked feature" })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Create token" })).not.toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "SCIM groups" })).not.toBeInTheDocument();

  await user.click(await screen.findByRole("button", { name: "Turn off SCIM" }));
  await waitFor(() => {
    expect(patched).toEqual({ enabled: false });
  });

  const table = await screen.findByRole("table", { name: "SCIM tokens" });
  await user.click(within(table).getByRole("button", { name: "Revoke" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.type(within(dialog).getByRole("textbox"), "revoke");
  await user.click(within(dialog).getByRole("button", { name: "Revoke" }));

  await waitFor(() => {
    expect(revokedId).toBe(scimFixture.tokens[0]?.id);
  });
});
