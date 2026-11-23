import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ENTITY_ID_CHANGE_DETAIL } from "@/ee/org-sso/saml";
import { server } from "@/mocks/node";
import { ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import meFixture from "../../../../../fixtures/api/static/me.json";
import entitlementsFixture from "../../../../../fixtures/api/static/org-entitlements.json";
import ssoFixture from "../../../../../fixtures/api/static/org-sso.json";

const DAY_MS = 86_400_000;
const IDP_ENTITY_ID = "https://idp.acme.test/metadata";
const METADATA_URL = "https://idp.acme.test/metadata";
const PEM_A = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
const PEM_B = "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----";

const samlSettings: Schema<"OrgSsoSaml"> = {
  idp_entity_id: IDP_ENTITY_ID,
  sso_url: "https://idp.acme.test/sso",
  metadata_url: METADATA_URL,
  certificates: [
    {
      sha256: "a".repeat(64),
      not_before: "2025-01-01T00:00:00Z",
      not_after: "2099-01-01T00:00:00Z",
    },
  ],
  attributes: {},
  refreshed_at: "2026-01-01T00:00:00Z",
};

const samlProvider: Schema<"OrgSsoProvider"> = {
  protocol: "saml",
  display_name: "Acme SAML",
  enabled: false,
  enforced: false,
  jit_role: "viewer",
  created_at: "2026-01-01T00:00:00.000000000Z",
  updated_at: "2026-01-01T00:00:00.000000000Z",
  saml: samlSettings,
};

const samlSso: Schema<"OrgSso"> = {
  ...ssoFixture,
  provider: samlProvider,
  domains: ssoFixture.domains as Schema<"OrgSso">["domains"],
};

function meAs(role: string) {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: meFixture.user,
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role }],
    }),
  );
}

/** Renders the page with the org's SSO answer and `features` added to the entitlements. */
async function openPage({
  sso = samlSso,
  features = ["org_sso", "saml"],
  role = "owner",
}: { sso?: object; features?: string[]; role?: string } = {}) {
  server.use(
    meAs(role),
    http.get("/api/v1/orgs/:org/sso", () => HttpResponse.json(sso)),
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({
        ...entitlementsFixture,
        features: [...entitlementsFixture.features, ...features],
      }),
    ),
  );
  return renderApp("/acme/settings/sso");
}

/** Records every SAML PUT body and answers with `answer`. */
function recordSaml(bodies: Record<string, unknown>[], answer: object = samlSso) {
  return http.put("/api/v1/orgs/:org/sso/saml", async ({ request }) => {
    bodies.push((await request.json()) as Record<string, unknown>);
    return HttpResponse.json(answer);
  });
}

it("sets up a SAML provider from a metadata URL", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(recordSaml(bodies));

  const { user } = await openPage({ sso: ssoFixture });
  await user.click(await screen.findByRole("radio", { name: "SAML 2.0" }));
  const name = screen.getByLabelText("Display name");
  await user.clear(name);
  await user.type(name, "Acme SAML");
  await user.type(screen.getByRole("textbox", { name: "Metadata URL" }), METADATA_URL);
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  expect(bodies[0]).toEqual({
    display_name: "Acme SAML",
    jit_role: "viewer",
    metadata_url: METADATA_URL,
  });
  // The protocol change remounts the form, so the saved SAML provider's cards show.
  expect(
    await screen.findByRole("heading", { name: "Identity provider metadata", level: 2 }),
  ).toBeInTheDocument();
  expect(screen.getByRole("list", { name: "Trusted certificates" })).toBeInTheDocument();
});

it("warns before switching the protocol", async () => {
  const { user } = await openPage({ sso: ssoFixture });
  await user.click(await screen.findByRole("radio", { name: "SAML 2.0" }));

  expect(
    screen.getByText(
      "Saving replaces the current provider. Members link again at their next sign-in, and the provider must be tested and enabled again.",
    ),
  ).toBeInTheDocument();
});

it("refuses a protocol switch while enforced", async () => {
  const enforced = { ...ssoFixture, provider: { ...ssoFixture.provider, enforced: true } };
  const { user } = await openPage({ sso: enforced });
  await user.click(await screen.findByRole("radio", { name: "SAML 2.0" }));

  expect(
    screen.getByText("Turn off enforcement before switching the protocol."),
  ).toBeInTheDocument();
  expect(screen.queryByText(/Saving replaces the current provider/)).not.toBeInTheDocument();
});

it("shows the service provider details", async () => {
  await openPage();

  await screen.findByRole("heading", { name: "Service provider details", level: 2 });
  expect(document.getElementById("saml-entity-id")).toHaveTextContent(
    ssoFixture.saml_sp.entity_id,
  );
  expect(document.getElementById("saml-acs-url")).toHaveTextContent(ssoFixture.saml_sp.acs_url);
  expect(document.getElementById("saml-metadata-url")).toHaveTextContent(
    ssoFixture.saml_sp.entity_id,
  );
  expect(document.getElementById("saml-start-url")).toHaveTextContent(
    ssoFixture.saml_sp.start_url,
  );
  expect(screen.getByRole("button", { name: "Copy acs url" })).toBeInTheDocument();
});

it("sends pasted metadata XML", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(recordSaml(bodies));

  const { user } = await openPage({ sso: ssoFixture });
  await user.click(await screen.findByRole("radio", { name: "SAML 2.0" }));
  await user.click(screen.getByRole("tab", { name: "Metadata XML" }));
  await user.click(screen.getByLabelText("Metadata XML"));
  await user.paste("<EntityDescriptor/>");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  expect(bodies[0]).toEqual({
    display_name: "Acme Okta",
    jit_role: "viewer",
    metadata_xml: "<EntityDescriptor/>",
  });
});

it("sends manual fields and every pasted certificate", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(recordSaml(bodies));

  const { user } = await openPage({ sso: ssoFixture });
  await user.click(await screen.findByRole("radio", { name: "SAML 2.0" }));
  await user.click(screen.getByRole("tab", { name: "Enter manually" }));
  await user.type(screen.getByRole("textbox", { name: "IdP entity ID" }), IDP_ENTITY_ID);
  await user.type(
    screen.getByRole("textbox", { name: "Sign-on URL" }),
    "https://idp.acme.test/sso",
  );
  await user.click(screen.getByLabelText("Signing certificates"));
  await user.paste(`${PEM_A}\n${PEM_B}`);
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  expect(bodies[0]).toEqual({
    display_name: "Acme Okta",
    jit_role: "viewer",
    idp_entity_id: IDP_ENTITY_ID,
    sso_url: "https://idp.acme.test/sso",
    certificates: [PEM_A, PEM_B],
  });
});

it("keeps the current metadata when only the name changes", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(
    http.put("/api/v1/orgs/:org/sso/saml", async ({ request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      bodies.push(body);
      // Echo the saved name, so the form is clean again after the save.
      return HttpResponse.json({
        ...samlSso,
        provider: { ...samlProvider, display_name: String(body.display_name) },
      });
    }),
  );

  const { user } = await openPage();
  await user.type(await screen.findByLabelText("Display name"), " EMEA");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  expect(bodies[0]).toEqual({ display_name: "Acme SAML EMEA", jit_role: "viewer" });
  expect(await screen.findByRole("status")).toHaveTextContent("Saved");
});

it("re-imports an unchanged metadata URL to accept a refresh failure", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(recordSaml(bodies));
  const failing: Schema<"OrgSso"> = {
    ...samlSso,
    provider: {
      ...samlProvider,
      saml: { ...samlSettings, refresh_error: "entity_id_changed" },
    },
  };

  const { user } = await openPage({ sso: failing });
  expect(await screen.findByText("Refresh failed")).toBeInTheDocument();
  await user.click(screen.getByRole("tab", { name: "Metadata URL" }));
  expect(screen.getByRole("textbox", { name: "Metadata URL" })).toHaveValue(METADATA_URL);
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  expect(bodies[0]).toEqual({
    display_name: "Acme SAML",
    jit_role: "viewer",
    metadata_url: METADATA_URL,
  });
});

it("asks for a client secret when switching from SAML to OpenID Connect", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(
    http.put("/api/v1/orgs/:org/sso/provider", async ({ request }) => {
      bodies.push((await request.json()) as Record<string, unknown>);
      return HttpResponse.json(ssoFixture);
    }),
  );

  const { user } = await openPage();
  await user.click(await screen.findByRole("radio", { name: "OpenID Connect" }));
  await user.type(screen.getByLabelText("Issuer URL"), "https://acme.okta.com");
  await user.type(screen.getByLabelText("Client ID"), "0oa1b2c3d4");
  await user.type(screen.getByLabelText("Client secret"), "x");
  await user.clear(screen.getByLabelText("Client secret"));

  expect(await screen.findByText("Enter a client secret.")).toBeInTheDocument();
  // The Save button stays off while a required field is empty, so no request is sent.
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  expect(bodies).toHaveLength(0);
});

it("sends only the attribute names that are set", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(recordSaml(bodies));

  const { user } = await openPage();
  await user.type(await screen.findByLabelText("Email attribute"), "mail");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  expect(bodies[0]).toEqual({
    display_name: "Acme SAML",
    jit_role: "viewer",
    attributes: { email: "mail" },
  });
});

it("confirms an entity ID change before relinking", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(
    http.put("/api/v1/orgs/:org/sso/saml", async ({ request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      bodies.push(body);
      if (body.confirm_entity_id_change !== true) {
        return problemResponse(409, "conflict", { detail: ENTITY_ID_CHANGE_DETAIL });
      }
      return HttpResponse.json(samlSso);
    }),
  );

  const { user } = await openPage();
  await user.type(await screen.findByLabelText("Display name"), " EMEA");
  await user.click(screen.getByRole("button", { name: "Save" }));

  const dialog = await screen.findByRole("alertdialog", { name: "Change the identity provider?" });
  expect(within(dialog).getByText(`Current IdP entity ID: ${IDP_ENTITY_ID}`)).toBeInTheDocument();
  expect(bodies).toHaveLength(1);
  await user.click(within(dialog).getByRole("button", { name: "Save and relink" }));

  await waitFor(() => {
    expect(bodies).toHaveLength(2);
  });
  expect(bodies[1]).toEqual({
    display_name: "Acme SAML EMEA",
    jit_role: "viewer",
    confirm_entity_id_change: true,
  });
});

it("shows no error in the form after the entity ID change is cancelled", async () => {
  const bodies: Record<string, unknown>[] = [];
  server.use(
    http.put("/api/v1/orgs/:org/sso/saml", async ({ request }) => {
      bodies.push((await request.json()) as Record<string, unknown>);
      return problemResponse(409, "conflict", { detail: ENTITY_ID_CHANGE_DETAIL });
    }),
  );

  const { user } = await openPage();
  await user.type(await screen.findByLabelText("Display name"), " EMEA");
  await user.click(screen.getByRole("button", { name: "Save" }));

  const dialog = await screen.findByRole("alertdialog", { name: "Change the identity provider?" });
  await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

  await waitFor(() => {
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  });
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(bodies).toHaveLength(1);
});

it("shows field errors under their fields", async () => {
  server.use(
    http.put("/api/v1/orgs/:org/sso/saml", () =>
      problemResponse(400, "bad_request", {
        errors: [
          {
            field: "metadata_url",
            message: "metadata URL must be https, or http on a loopback host",
          },
          { field: "attributes.email", message: "invalid attribute name" },
        ],
      }),
    ),
  );

  const { user } = await openPage();
  await user.click(await screen.findByRole("tab", { name: "Metadata URL" }));
  const url = screen.getByRole("textbox", { name: "Metadata URL" });
  await user.clear(url);
  await user.type(url, "http://idp.acme.test/metadata");
  await user.type(screen.getByLabelText("Email attribute"), "bad name");
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(
    await screen.findByText("metadata URL must be https, or http on a loopback host"),
  ).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Metadata URL" })).toHaveAttribute(
    "aria-invalid",
    "true",
  );
  expect(screen.getByText("invalid attribute name")).toBeInTheDocument();
  expect(screen.getByLabelText("Email attribute")).toHaveAttribute("aria-invalid", "true");
});

it("shows a metadata source error under the tabs", async () => {
  const message =
    "give a metadata URL, the metadata XML, or the entity ID, SSO URL and certificates";
  server.use(
    http.put("/api/v1/orgs/:org/sso/saml", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "metadata", message }],
      }),
    ),
  );

  const { user } = await openPage();
  await user.click(await screen.findByRole("tab", { name: "Metadata URL" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(message);
});

it("shows certificate expiry and refresh failures", async () => {
  const now = Date.now();
  const certificates: Schema<"OrgSsoCertificate">[] = [
    {
      sha256: "b".repeat(64),
      not_before: "2025-01-01T00:00:00Z",
      not_after: new Date(now + 10 * DAY_MS).toISOString(),
    },
    {
      sha256: "c".repeat(64),
      not_before: "2020-01-01T00:00:00Z",
      not_after: "2021-01-01T00:00:00Z",
    },
    {
      sha256: "d".repeat(64),
      not_before: "1970-01-01T00:00:00Z",
      not_after: "1970-01-01T00:00:00Z",
    },
  ];
  const sso: Schema<"OrgSso"> = {
    ...samlSso,
    provider: {
      ...samlProvider,
      saml: {
        ...samlSettings,
        certificates,
        refresh_error: "entity_id_changed",
        certificates_changed_at: "2026-03-01T00:00:00Z",
      },
    },
  };
  await openPage({ sso });

  expect(await screen.findByText("Expires in 10 days")).toBeInTheDocument();
  expect(screen.getByText("Expired")).toBeInTheDocument();
  expect(screen.getByText("Unreadable")).toBeInTheDocument();
  expect(screen.getByText("Refresh failed")).toBeInTheDocument();
  expect(
    screen.getByText(
      "The metadata now names another entity ID. Save it again to accept the change.",
    ),
  ).toBeInTheDocument();
  expect(screen.getByText(/Certificates changed on/)).toBeInTheDocument();
});

it("locks SAML without the saml feature and keeps turning the provider off", async () => {
  let patched: unknown;
  server.use(
    http.patch("/api/v1/orgs/:org/sso", async ({ request }) => {
      patched = await request.json();
      return HttpResponse.json({
        ...samlSso,
        provider: { ...samlProvider, enabled: false },
      });
    }),
  );
  const enabled = { ...samlSso, provider: { ...samlProvider, enabled: true } };

  const { user } = await openPage({ sso: enabled, features: ["org_sso"] });

  const note = await screen.findByRole("note", { name: "Locked feature" });
  expect(note).toHaveTextContent("Available with an enterprise licence that includes SAML");
  expect(screen.getByLabelText("Display name")).toBeDisabled();
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  expect(screen.getByRole("switch", { name: "Enforced" })).toBeDisabled();

  await user.click(screen.getByRole("switch", { name: "Enabled" }));
  await waitFor(() => {
    expect(patched).toEqual({ enabled: false });
  });
});

it("shows the SAML lock when choosing SAML over OpenID Connect", async () => {
  const { user } = await openPage({ sso: ssoFixture, features: ["org_sso"] });
  await user.click(await screen.findByRole("radio", { name: "SAML 2.0" }));

  expect(await screen.findByRole("note", { name: "Locked feature" })).toHaveTextContent(
    "Available with an enterprise licence that includes SAML",
  );
});

it("removal asks for the IdP entity ID", async () => {
  const { user } = await openPage();
  await user.click(await screen.findByRole("button", { name: "Remove provider…" }));

  const dialog = await screen.findByRole("alertdialog");
  expect(
    within(dialog).getByLabelText(`Type "${IDP_ENTITY_ID}" to confirm`),
  ).toBeInTheDocument();
});

it("asks before leaving the SAML form with unsaved changes", async () => {
  const { user, router } = await openPage();
  await user.type(await screen.findByLabelText("Display name"), " EMEA");

  void router.navigate({ to: "/$org/settings/general", params: { org: "acme" } });

  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  expect(router.state.location.pathname).toBe("/acme/settings/sso");
  expect(screen.getByLabelText("Display name")).toHaveValue("Acme SAML EMEA");
});
