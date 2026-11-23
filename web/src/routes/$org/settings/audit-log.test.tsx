import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { ADA_ID, ORG_ID } from "@/test/fixtures";
import { renderApp } from "@/test/render";
import auditFixture from "../../../../../fixtures/api/static/org-audit-events.json";
import auditStreamFixture from "../../../../../fixtures/api/static/org-audit-stream.json";
import entitlementsFixture from "../../../../../fixtures/api/static/org-entitlements.json";

const originalCreate = URL.createObjectURL;
const originalRevoke = URL.revokeObjectURL;

afterEach(() => {
  URL.createObjectURL = originalCreate;
  URL.revokeObjectURL = originalRevoke;
});

function signedInAs(role: "owner" | "admin" | "member") {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: { id: ADA_ID, email: "ada@example.com", name: "Ada Example", is_instance_admin: false },
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role }],
    }),
  );
}

/** The org entitlements with the audit log included, or without it. */
function withFeatures(features: string[]) {
  return http.get("/api/v1/orgs/:org/entitlements", () =>
    HttpResponse.json({ ...entitlementsFixture, features }),
  );
}

it("shows admins the records with names and the former-member label", async () => {
  server.use(signedInAs("admin"), withFeatures(["audit_log"]));

  await renderApp("/acme/settings/audit-log");

  const table = await screen.findByRole("table", { name: "Audit records" });
  expect(within(table).getAllByText("Ada Example").length).toBeGreaterThan(0);
  expect(within(table).getAllByText("ada@example.com").length).toBeGreaterThan(0);
  expect(within(table).getByText("Former member")).toBeInTheDocument();
  expect(within(table).getByRole("button", { name: "member.role_changed" })).toBeInTheDocument();
});

it("shows a present actor's email when it has no name", async () => {
  // An invited user has an email before it sets a display name.
  const record: Schema<"AuditEvent"> = {
    id: "01900000-0000-7000-8000-000000001d04",
    at: "2026-01-04T07:00:00.000000000Z",
    action: "member.invited",
    outcome: "success",
    actor: {
      kind: "user",
      id: "01900000-0000-7000-8000-000000000103",
      email: "noname@example.com",
      status: "present",
    },
    details: {},
  };
  server.use(
    signedInAs("admin"),
    withFeatures(["audit_log"]),
    http.get("/api/v1/orgs/:org/audit-events", () =>
      HttpResponse.json({ data: [record], next_cursor: null }),
    ),
  );

  await renderApp("/acme/settings/audit-log");

  const table = await screen.findByRole("table", { name: "Audit records" });
  // The record has no target, so the Actor column is the only place its email can be shown.
  expect(within(table).getAllByText("noname@example.com")).toHaveLength(1);
  expect(within(table).queryByText("user")).toBeNull();
});

it("sends the filters to the URL and to the query", async () => {
  const actorIds: (string | null)[] = [];
  server.use(
    signedInAs("owner"),
    withFeatures(["audit_log"]),
    http.get("/api/v1/orgs/:org/audit-events", ({ request }) => {
      actorIds.push(new URL(request.url).searchParams.get("actor_id"));
      return HttpResponse.json(auditFixture);
    }),
  );

  const { router, user } = await renderApp("/acme/settings/audit-log");

  await user.type(await screen.findByLabelText("Actor ID"), ADA_ID);
  await waitFor(() => expect(router.state.location.search).toMatchObject({ actor: ADA_ID }));
  await waitFor(() => expect(actorIds).toContain(ADA_ID));
});

it("opens a record's details from its row", async () => {
  server.use(signedInAs("admin"), withFeatures(["audit_log"]));

  const { router, user } = await renderApp("/acme/settings/audit-log");

  await user.click(await screen.findByRole("button", { name: "member.role_changed" }));

  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByText(/"new_role": "admin"/)).toBeInTheDocument();
  expect(within(dialog).getByRole("button", { name: "Copy Audit record ID" })).toBeInTheDocument();
  expect(router.state.location.search).toMatchObject({
    open: "01900000-0000-7000-8000-000000001d01",
  });
});

it("tells members that only admins and owners can read the log", async () => {
  server.use(signedInAs("member"), withFeatures(["audit_log"]));

  await renderApp("/acme/settings/audit-log");

  expect(
    await screen.findByText("Only admins and owners can read the audit log."),
  ).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Audit records" })).toBeNull();
});

it("shows the lock without the audit_log feature", async () => {
  server.use(signedInAs("owner"), withFeatures([]));

  await renderApp("/acme/settings/audit-log");

  const lock = await screen.findByRole("note", { name: "Locked feature" });
  expect(lock).toHaveTextContent("Available with Enterprise or an enterprise licence.");
  expect(screen.queryByRole("table", { name: "Audit records" })).toBeNull();
});

it("admins who are not owners do not see the stream card, and the log stays", async () => {
  // The fixture mocks answer every GET, so "no card" is not proof that the page skipped the
  // request: the handler counts.
  let streamGets = 0;
  server.use(
    signedInAs("admin"),
    withFeatures(["audit_log"]),
    http.get("/api/v1/orgs/:org/audit-stream", () => {
      streamGets += 1;
      return HttpResponse.json(auditStreamFixture);
    }),
  );

  await renderApp("/acme/settings/audit-log");

  expect(await screen.findByRole("table", { name: "Audit records" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Streaming" })).toBeNull();
  expect(screen.queryByLabelText("URL")).toBeNull();
  expect(streamGets).toBe(0);
});

it("shows owners the stream card above the records", async () => {
  server.use(signedInAs("owner"), withFeatures(["audit_log"]));

  await renderApp("/acme/settings/audit-log");

  const card = await screen.findByRole("region", { name: "Streaming" });
  expect(card).toHaveTextContent("siem.example.com");
  // Above the filters, so the card comes first in reading order.
  const table = await screen.findByRole("table", { name: "Audit records" });
  expect(card.compareDocumentPosition(table)).toEqual(Node.DOCUMENT_POSITION_FOLLOWING);
});

it("exports the filtered records as CSV", async () => {
  URL.createObjectURL = vi.fn(() => "blob:audit");
  URL.revokeObjectURL = vi.fn();
  const formats: (string | null)[] = [];
  server.use(
    signedInAs("admin"),
    withFeatures(["audit_log"]),
    http.get("/api/v1/orgs/:org/audit-events/export", ({ request }) => {
      formats.push(new URL(request.url).searchParams.get("format"));
      return new HttpResponse("id,action\n", {
        headers: {
          "Content-Type": "text/csv",
          "Content-Disposition": 'attachment; filename="audit-acme.csv"',
        },
      });
    }),
  );

  const { user } = await renderApp("/acme/settings/audit-log");

  await user.click(await screen.findByRole("button", { name: "Export" }));
  await user.click(await screen.findByRole("menuitem", { name: "Export CSV" }));

  await waitFor(() => expect(formats).toEqual(["csv"]));
  await waitFor(() => expect(URL.createObjectURL).toHaveBeenCalled());
});
