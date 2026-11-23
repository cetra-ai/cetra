import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ADA_ID, ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import auditStreamFixture from "../../../../fixtures/api/static/admin-audit-stream.json";

function signedInAsInstanceAdmin() {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: { id: ADA_ID, email: "ada@example.com", name: "Ada Example", is_instance_admin: true },
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role: "owner" }],
    }),
  );
}

it("shows an instance admin the instance log", async () => {
  server.use(signedInAsInstanceAdmin());

  await renderApp("/admin/audit-log");

  const table = await screen.findByRole("table", { name: "Audit records" });
  expect(within(table).getByRole("button", { name: "user.created" })).toBeInTheDocument();
  expect(within(table).getByRole("button", { name: "auth.login_failed" })).toBeInTheDocument();
});

it("says the instance log needs a licence when the server refuses it", async () => {
  server.use(
    signedInAsInstanceAdmin(),
    http.get("/api/v1/admin/audit-events", () =>
      problemResponse(403, "plan_required", { detail: "The licence does not list audit_log." }),
    ),
  );

  await renderApp("/admin/audit-log");

  expect(
    await screen.findByText(
      /The instance audit log needs an enterprise licence that lists audit_log\./,
    ),
  ).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "See the Settings tab" })).toHaveAttribute(
    "href",
    "/admin/settings",
  );
});

it("instance admin manages the instance destination", async () => {
  const bodies: unknown[] = [];
  const tested: string[] = [];
  server.use(
    signedInAsInstanceAdmin(),
    http.put("/api/v1/admin/audit-stream", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json(auditStreamFixture);
    }),
    http.post("/api/v1/admin/audit-stream/test", () => {
      tested.push("test");
      return HttpResponse.json({ reached: true, status: 201 });
    }),
  );

  const { user } = await renderApp("/admin/audit-log");

  const card = await screen.findByRole("region", { name: "Streaming" });
  expect(card).toHaveTextContent("the instance's audit records");
  // The instance fixture fails: its error class, and the records behind.
  expect(card).toHaveTextContent("the endpoint answered 503");
  expect(card).toHaveTextContent("12 records behind");

  await user.click(within(card).getByRole("button", { name: "Edit" }));
  // The URL is never returned: the edit asks for it again.
  expect(screen.getByLabelText("URL")).toHaveValue("");
  await user.type(screen.getByLabelText("URL"), "https://siem.example.com/ingest");
  await user.click(within(card).getByRole("switch", { name: "Enabled" }));
  await user.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => {
    expect(bodies).toEqual([{ url: "https://siem.example.com/ingest", enabled: false }]);
  });

  await user.click(within(card).getByRole("button", { name: "Send test" }));
  expect(await screen.findByText("Reached, status 201")).toBeInTheDocument();
  expect(tested).toEqual(["test"]);
});
