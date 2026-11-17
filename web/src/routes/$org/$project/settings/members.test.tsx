import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ADA_ID, BOB_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const projectNav = () => within(screen.getByRole("list", { name: "Project settings" }));

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

const BOB = { id: BOB_ID, email: "bob@example.com", name: "Bob Example" };
const ADA = { id: ADA_ID, email: "ada@example.com", name: "Ada Example" };

const BOB_PANEL = `/acme/support-bot/settings/members?open=${BOB_ID}`;
const ADA_PANEL = `/acme/support-bot/settings/members?open=${ADA_ID}`;

/** The access picker of the open panel. */
function bobAccess() {
  return screen.findByRole("combobox", { name: "Project access of bob@example.com" });
}

const projectRolesLocked = () =>
  http.get("/api/v1/orgs/:org/entitlements", () =>
    HttpResponse.json({ features: [], upgrade_url: "https://up.example/billing" }),
  );

it("shows effective roles and overrides", async () => {
  await renderApp("/acme/support-bot/settings/members");

  const table = await screen.findByRole("grid", { name: "Project members" });
  const ada = within(table).getByRole("row", { name: /ada@example.com/ });
  const adaCells = within(ada).getAllByRole("gridcell");
  expect(adaCells[0]).toHaveTextContent("Ada Example");
  expect(adaCells[1]).toHaveTextContent("Owner");
  expect(adaCells[2]).toHaveTextContent("Always admin");
  expect(adaCells[3]).toHaveTextContent("Admin");

  const bob = within(table).getByRole("row", { name: /bob@example.com/ });
  const bobCells = within(bob).getAllByRole("gridcell");
  expect(bobCells[2]).toHaveTextContent("Viewer");
  expect(bobCells[3]).toHaveTextContent("Viewer");
});

it("opens the panel from a row", async () => {
  const { user } = await renderApp("/acme/support-bot/settings/members");
  const table = await screen.findByRole("grid", { name: "Project members" });
  await user.click(within(table).getByRole("row", { name: /bob@example.com/ }));

  expect(await screen.findByRole("heading", { name: "Bob Example" })).toBeInTheDocument();
  expect(await bobAccess()).toHaveTextContent("Viewer");
});

it("sets an override with Save", async () => {
  let pathUser = "";
  let body: unknown;
  server.use(
    http.put("/api/v1/projects/:project/members/:user", async ({ params, request }) => {
      pathUser = String(params.user);
      body = await request.json();
      return HttpResponse.json({ user: BOB, org_role: "member", override: "none" });
    }),
  );

  const { user } = await renderApp(BOB_PANEL);
  await user.click(await bobAccess());
  await user.click(await screen.findByRole("option", { name: "No access" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ role: "none" }));
  expect(pathUser).toBe(BOB_ID);
});

it("removes an override with Inherit", async () => {
  const users: string[] = [];
  server.use(
    http.delete("/api/v1/projects/:project/members/:user", ({ params }) => {
      users.push(String(params.user));
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await renderApp(BOB_PANEL);
  await user.click(await bobAccess());
  await user.click(await screen.findByRole("option", { name: "Inherit org role" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(users).toEqual([BOB_ID]));
});

it("shows the server's refusal", async () => {
  server.use(
    http.put("/api/v1/projects/:project/members/:user", () =>
      problemResponse(400, "bad_request", {
        detail: "owners and admins cannot be overridden",
      }),
    ),
  );

  const { user } = await renderApp(BOB_PANEL);
  await user.click(await bobAccess());
  await user.click(await screen.findByRole("option", { name: "Admin" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByText("owners and admins cannot be overridden")).toBeInTheDocument();
});

it("explains a 403", async () => {
  server.use(
    http.put("/api/v1/projects/:project/members/:user", () => problemResponse(403, "forbidden")),
  );

  const { user } = await renderApp(BOB_PANEL);
  await user.click(await bobAccess());
  await user.click(await screen.findByRole("option", { name: "Member" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByText("Only project admins can change access.")).toBeInTheDocument();
});

it("refreshes the caller's project role after changing their own access", async () => {
  let projectCalls = 0;
  server.use(
    http.get("/api/v1/projects/:project", () => {
      projectCalls += 1;
      return HttpResponse.json({ ...projectFixture, role: "admin" });
    }),
    http.get("/api/v1/projects/:project/members", () =>
      HttpResponse.json({
        data: [
          { user: ADA, org_role: "member", override: "admin", effective_role: "admin" },
          { user: BOB, org_role: "member", override: "viewer", effective_role: "viewer" },
        ],
        next_cursor: null,
      }),
    ),
    http.put("/api/v1/projects/:project/members/:user", () =>
      HttpResponse.json({
        user: ADA,
        org_role: "member",
        override: "member",
        effective_role: "member",
      }),
    ),
  );

  const { user } = await renderApp(ADA_PANEL);
  await user.click(
    await screen.findByRole("combobox", { name: "Project access of ada@example.com" }),
  );
  await user.click(await screen.findByRole("option", { name: "Member" }));
  const before = projectCalls;
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(projectCalls).toBeGreaterThan(before));
});

it("loads more members", async () => {
  server.use(
    http.get("/api/v1/projects/:project/members", ({ request }) => {
      const cursor = new URL(request.url).searchParams.get("cursor");
      if (cursor === "c2") {
        return HttpResponse.json({
          data: [
            {
              user: {
                id: "01900000-0000-7000-8000-000000000103",
                email: "carol@example.com",
                name: "Carol Example",
              },
              org_role: "viewer",
              effective_role: "viewer",
            },
          ],
          next_cursor: null,
        });
      }
      return HttpResponse.json({
        data: [
          { user: ADA, org_role: "owner", effective_role: "admin" },
          { user: BOB, org_role: "member", override: "viewer", effective_role: "viewer" },
        ],
        next_cursor: "c2",
      });
    }),
  );

  await renderApp("/acme/support-bot/settings/members");

  expect(await screen.findByRole("row", { name: /carol@example.com/ })).toBeInTheDocument();
});

it("searches members by name or email", async () => {
  const { user } = await renderApp("/acme/support-bot/settings/members");
  await screen.findByRole("grid", { name: "Project members" });
  await user.type(screen.getByRole("searchbox", { name: "Search project members" }), "ada@");

  const table = screen.getByRole("grid", { name: "Project members" });
  expect(within(table).getByRole("row", { name: /ada@example.com/ })).toBeInTheDocument();
  expect(within(table).queryByRole("row", { name: /bob@example.com/ })).toBeNull();
});

it("shows an empty result when no member matches", async () => {
  const { user } = await renderApp("/acme/support-bot/settings/members");
  await screen.findByRole("grid", { name: "Project members" });
  await user.type(screen.getByRole("searchbox", { name: "Search project members" }), "zzz");

  expect(await screen.findByText("No member matches")).toBeInTheDocument();
});

it("sorts by name from the URL", async () => {
  await renderApp("/acme/support-bot/settings/members?sort=name&desc=true");

  const table = await screen.findByRole("grid", { name: "Project members" });
  const names = within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) => within(row).getAllByRole("gridcell")[0]?.textContent);
  expect(names[0]).toContain("Bob Example");
  expect(names[1]).toContain("Ada Example");
});

it("is read-only for a viewer", async () => {
  server.use(projectAs("viewer"));

  const { user } = await renderApp(BOB_PANEL);

  const access = await bobAccess();
  expect(access).toBeDisabled();
  expect(access.parentElement).toHaveAccessibleDescription("Only project admins can change access");
  expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
  expect(projectNav().getByRole("link", { name: "General" })).toBeInTheDocument();
  expect(projectNav().getByRole("link", { name: "Members" })).toBeInTheDocument();
  expect(projectNav().queryByRole("link", { name: "API keys" })).toBeNull();
  await user.click(access);
  expect(screen.queryByRole("option", { name: "Admin" })).toBeNull();
});

it("shows the lock and keeps removal when project roles are locked", async () => {
  const users: string[] = [];
  server.use(
    projectRolesLocked(),
    http.delete("/api/v1/projects/:project/members/:user", ({ params }) => {
      users.push(String(params.user));
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await renderApp(BOB_PANEL);

  expect(await screen.findByRole("note", { name: "Locked feature" })).toHaveTextContent(
    "Available with the Teams add-on or an enterprise licence.",
  );
  expect(screen.getByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    "https://up.example/billing",
  );

  await user.click(await bobAccess());
  for (const name of ["Admin", "Member", "No access"]) {
    expect(await screen.findByRole("option", { name })).toHaveAttribute("aria-disabled", "true");
  }
  expect(screen.getByRole("option", { name: "Inherit org role" })).not.toHaveAttribute(
    "aria-disabled",
    "true",
  );
  expect(screen.getByRole("option", { name: "Viewer" })).not.toHaveAttribute(
    "aria-disabled",
    "true",
  );

  await user.click(screen.getByRole("option", { name: "Inherit org role" }));
  await user.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(users).toEqual([BOB_ID]));
});

it("shows no lock when project roles are allowed", async () => {
  await renderApp(BOB_PANEL);
  await bobAccess();

  expect(screen.queryByRole("note", { name: "Locked feature" })).toBeNull();
});

it("shows no lock while the entitlements are loading", async () => {
  server.use(http.get("/api/v1/orgs/:org/entitlements", () => new Promise<Response>(() => {})));

  const { user } = await renderApp(BOB_PANEL);

  expect(screen.queryByRole("note", { name: "Locked feature" })).toBeNull();
  await user.click(await bobAccess());
  for (const name of ["Admin", "Member", "No access"]) {
    expect(await screen.findByRole("option", { name })).not.toHaveAttribute(
      "aria-disabled",
      "true",
    );
  }
});

it("shows the plan message from the server", async () => {
  server.use(
    http.put("/api/v1/projects/:project/members/:user", () =>
      problemResponse(403, "plan_required", {
        detail: "Per-project roles need the Teams add-on or an enterprise licence.",
      }),
    ),
  );

  const { user } = await renderApp(BOB_PANEL);
  await user.click(await bobAccess());
  await user.click(await screen.findByRole("option", { name: "Admin" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(
    await screen.findByText("Per-project roles need the Teams add-on or an enterprise licence."),
  ).toBeInTheDocument();
});

it("shows no lock to a viewer", async () => {
  server.use(projectAs("viewer"), projectRolesLocked());

  await renderApp(BOB_PANEL);
  await bobAccess();

  expect(screen.queryByRole("note", { name: "Locked feature" })).toBeNull();
});

it("asks before discarding an unsaved override", async () => {
  const { user } = await renderApp(BOB_PANEL);
  await user.click(await bobAccess());
  await user.click(await screen.findByRole("option", { name: "No access" }));

  await user.click(projectNav().getByRole("link", { name: "General" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));

  await waitFor(() =>
    expect(screen.queryByRole("alertdialog", { name: "Discard unsaved changes?" })).toBeNull(),
  );
  expect(await bobAccess()).toHaveTextContent("No access");
});

it("says when the member is not in the loaded list", async () => {
  await renderApp("/acme/support-bot/settings/members?open=01900000-0000-7000-8000-000000000999");

  expect(await screen.findByRole("heading", { name: "Member not found" })).toBeInTheDocument();
});
