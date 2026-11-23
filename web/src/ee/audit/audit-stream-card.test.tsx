// SPDX-License-Identifier: Elastic-2.0
import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ADA_ID, ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import orgStreamFixture from "../../../../fixtures/api/static/org-audit-stream.json";
import entitlementsFixture from "../../../../fixtures/api/static/org-entitlements.json";

const SECRET = "created-signing-secret-not-for-use";
const ROTATED = "rotated-signing-secret-not-for-use";

function signedInAsOwner() {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: { id: ADA_ID, email: "ada@example.com", name: "Ada Example", is_instance_admin: false },
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role: "owner" }],
    }),
  );
}

/** The org entitlements with the audit log included, so the page renders its view. */
function withAuditLog() {
  return http.get("/api/v1/orgs/:org/entitlements", () =>
    HttpResponse.json({
      ...entitlementsFixture,
      features: [...entitlementsFixture.features, "audit_log"],
    }),
  );
}

/** The org page as its owner, with the stream GET answering `stream` (the fixture when absent). */
async function openOrgPage(stream?: Record<string, unknown>) {
  server.use(
    signedInAsOwner(),
    withAuditLog(),
    ...(stream !== undefined
      ? [http.get("/api/v1/orgs/:org/audit-stream", () => HttpResponse.json(stream))]
      : []),
  );
  return renderApp("/acme/settings/audit-log");
}

it("owner creates a destination and sees the secret once", async () => {
  let put: unknown;
  server.use(
    http.get("/api/v1/orgs/:org/audit-stream", () => problemResponse(404, "not_found")),
    http.put("/api/v1/orgs/:org/audit-stream", async ({ request }) => {
      put = await request.json();
      return HttpResponse.json({ stream: orgStreamFixture, secret: SECRET }, { status: 201 });
    }),
  );

  const { user } = await openOrgPage();

  await user.type(await screen.findByLabelText("URL"), "https://siem.example.com/ingest");
  await user.type(screen.getByLabelText("Header name"), "Authorization");
  await user.type(screen.getByLabelText("Header value"), "Splunk token");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => {
    expect(put).toEqual({
      url: "https://siem.example.com/ingest",
      header_name: "Authorization",
      header_value: "Splunk token",
      enabled: true,
    });
  });
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByLabelText("Secret")).toHaveValue(SECRET);
  expect(within(dialog).getByText(/verify the X-Cetra-Signature header/)).toBeInTheDocument();

  await user.click(within(dialog).getByRole("button", { name: "Done" }));
  await waitFor(() => {
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
  expect(screen.queryByDisplayValue(SECRET)).toBeNull();
  // The card left the create form: the destination it created is shown.
  expect(screen.getByRole("region", { name: "Streaming" })).toHaveTextContent("siem.example.com");
});

it("status shows lag and errors", async () => {
  await openOrgPage({
    url_host: "siem.example.com",
    header_name: "Authorization",
    enabled: true,
    status: {
      last_success_at: "2026-11-23T12:00:00.000000000Z",
      last_error: "the endpoint answered 503",
      failing_since: "2026-11-23T11:50:00.000000000Z",
      lag: 12,
      lost_records: true,
    },
  });

  const card = await screen.findByRole("region", { name: "Streaming" });

  expect(card).toHaveTextContent("siem.example.com");
  expect(card).toHaveTextContent("Authorization");
  expect(card).toHaveTextContent("Last delivered");
  expect(card).toHaveTextContent("12 records behind");
  expect(card).toHaveTextContent("the endpoint answered 503");
  expect(card).toHaveTextContent("failing since");
  expect(card).toHaveTextContent("Some records were deleted by retention before they were sent.");
});

it("edit keeps the header value when blank", async () => {
  let put: unknown;
  server.use(
    http.put("/api/v1/orgs/:org/audit-stream", async ({ request }) => {
      put = await request.json();
      return HttpResponse.json(orgStreamFixture);
    }),
  );

  const { user } = await openOrgPage();

  const card = await screen.findByRole("region", { name: "Streaming" });
  await user.click(within(card).getByRole("button", { name: "Edit" }));
  expect(screen.getByLabelText("Header name")).toHaveValue("Authorization");
  expect(screen.getByLabelText("Header value")).toHaveAttribute(
    "placeholder",
    "Stored; leave blank to keep it",
  );
  await user.type(screen.getByLabelText("URL"), "https://siem.example.com/ingest");
  await user.click(screen.getByRole("button", { name: "Save" }));

  // No `header_value`: the server keeps the stored one under the same name.
  await waitFor(() => {
    expect(put).toEqual({
      url: "https://siem.example.com/ingest",
      header_name: "Authorization",
      enabled: true,
    });
  });
  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Edit" })).toBeInTheDocument();
  });
});

it("flipping the switch asks for the URL again", async () => {
  const { user } = await openOrgPage();

  const card = await screen.findByRole("region", { name: "Streaming" });
  expect(within(card).getByRole("switch", { name: "Enabled" })).toBeChecked();
  await user.click(within(card).getByRole("switch", { name: "Enabled" }));

  // PUT needs the URL, which cetra never returns: the form opens with the new value, and Save
  // stays disabled until the URL is typed again.
  expect(screen.getByRole("switch", { name: "Enabled" })).not.toBeChecked();
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  expect(screen.getByText("Enter a URL.")).toBeInTheDocument();
  await user.type(screen.getByLabelText("URL"), "https://siem.example.com/ingest");
  expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
});

it("rotating the secret shows the new one once", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/audit-stream/secret", () =>
      HttpResponse.json({ secret: ROTATED }),
    ),
  );

  const { user } = await openOrgPage();

  const card = await screen.findByRole("region", { name: "Streaming" });
  await user.click(within(card).getByRole("button", { name: "Rotate secret" }));

  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByRole("heading", { name: "New signing secret" })).toBeInTheDocument();
  expect(within(dialog).getByLabelText("Secret")).toHaveValue(ROTATED);
  await user.click(within(dialog).getByRole("button", { name: "Done" }));
  await waitFor(() => {
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
  expect(screen.queryByDisplayValue(ROTATED)).toBeNull();
});

it("removing the destination asks for confirmation, then the create form returns", async () => {
  let deletes = 0;
  server.use(
    http.delete("/api/v1/orgs/:org/audit-stream", () => {
      deletes += 1;
      return new HttpResponse(null, { status: 204 });
    }),
    // The refetch after the removal finds no destination.
    http.get("/api/v1/orgs/:org/audit-stream", () =>
      deletes === 0 ? HttpResponse.json(orgStreamFixture) : problemResponse(404, "not_found"),
    ),
  );

  const { user } = await openOrgPage();

  const card = await screen.findByRole("region", { name: "Streaming" });
  await user.click(within(card).getByRole("button", { name: "Remove" }));

  const dialog = await screen.findByRole("alertdialog");
  const confirm = within(dialog).getByRole("button", { name: "Remove" });
  expect(confirm).toBeDisabled();
  await user.type(within(dialog).getByRole("textbox"), "remove");
  await user.click(confirm);

  await waitFor(() => {
    expect(deletes).toBe(1);
  });
  expect(await screen.findByRole("button", { name: "Save" })).toBeInTheDocument();
  expect(screen.getByLabelText("URL")).toHaveValue("");
});
