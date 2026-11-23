import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ADA_ID, ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";

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
