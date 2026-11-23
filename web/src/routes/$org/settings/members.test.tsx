import { act, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { $api } from "@/api/client";
import { projectQuery } from "@/api/queries";
import { projectMembersInit } from "@/api/queries-settings";
import { server } from "@/mocks/node";
import { ADA_ID, BOB_ID, INVITE_ID, ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import authConfigFixture from "../../../../../fixtures/api/static/auth-config.json";
import inviteCreated from "../../../../../fixtures/api/static/invite-created.json";
import entitlementsFixture from "../../../../../fixtures/api/static/org-entitlements.json";

const ADA = { id: ADA_ID, email: "ada@example.com", name: "Ada Example" };
const BOB = { id: BOB_ID, email: "bob@example.com", name: "Bob Example" };

function meAs(role: string, orgs = [{ id: ORG_ID, slug: "acme", name: "Acme", role }]) {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({ user: { ...ADA, is_instance_admin: true }, orgs }),
  );
}

const MEMBERS = "/acme/settings/members";
const orgNav = () => within(screen.getByRole("list", { name: "Organization settings" }));

function searchOf(router: { state: { location: { search: unknown } } }) {
  return router.state.location.search as Record<string, unknown>;
}

/** Opens the panel of a member through its row. */
async function openMember(email: string, path = MEMBERS) {
  const rendered = await renderApp(path);
  const grid = await screen.findByRole("grid", { name: "Members" });
  const row = within(grid).getByRole("row", { name: new RegExp(email) });
  await rendered.user.click(within(row).getByText(email));
  const form = await screen.findByRole("form", { name: `Role of ${email}` });
  return { ...rendered, form };
}

/** Picks a role in a member's panel and saves it. */
async function changeRoleTo(
  user: Awaited<ReturnType<typeof renderApp>>["user"],
  form: HTMLElement,
  role: string,
) {
  await user.click(within(form).getByRole("combobox"));
  await user.click(await screen.findByRole("option", { name: role }));
  await user.click(within(form).getByRole("button", { name: "Save" }));
}

it("lists members and marks you", async () => {
  await renderApp(MEMBERS);

  const grid = await screen.findByRole("grid", { name: "Members" });
  expect(within(grid).getByRole("row", { name: /ada@example.com/ })).toHaveTextContent("You");
  expect(within(grid).getByRole("row", { name: /bob@example.com/ })).toBeInTheDocument();
  expect(orgNav().getByRole("link", { name: "Members" })).toHaveAttribute("aria-current", "page");
});

it("lists pending invitations on the Invites tab", async () => {
  const { user, router } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });

  await user.click(screen.getByRole("tab", { name: "Invites" }));

  const invites = await screen.findByRole("grid", { name: "Pending invitations" });
  expect(invites).toHaveTextContent("guest@example.com");
  expect(searchOf(router)).toMatchObject({ tab: "invites" });
});

it("opens a member's panel from its row and writes the id to the URL", async () => {
  const { form, router } = await openMember("bob@example.com");

  expect(searchOf(router)).toMatchObject({ open: BOB_ID });
  expect(within(form).getByRole("combobox")).toHaveTextContent("Member");
});

it("opens a member's panel from the URL", async () => {
  await renderApp(`${MEMBERS}?open=${BOB_ID}`);

  expect(await screen.findByRole("form", { name: "Role of bob@example.com" })).toBeInTheDocument();
});

it("switching members keeps one role card", async () => {
  const { user } = await openMember("bob@example.com");
  const grid = screen.getByRole("grid", { name: "Members" });
  const row = within(grid).getByRole("row", { name: /ada@example.com/ });
  await user.click(within(row).getByText("ada@example.com"));

  const form = await screen.findByRole("form", { name: "Role of ada@example.com" });
  expect(screen.getAllByRole("form", { name: /^Role of / })).toHaveLength(1);
  expect(within(form).getByRole("combobox")).toHaveTextContent("Owner");
});

it("filters members by name or email", async () => {
  const { user } = await renderApp(MEMBERS);
  const grid = await screen.findByRole("grid", { name: "Members" });
  const search = screen.getByRole("searchbox", { name: "Search members" });

  await user.type(search, "bob");
  await waitFor(() => expect(within(grid).queryByText("Ada Example")).toBeNull());
  expect(within(grid).getByText("Bob Example")).toBeInTheDocument();

  await user.clear(search);
  await user.type(search, "nobody");
  expect(await screen.findByText("No member matches.")).toBeInTheDocument();
});

it("sorts members by role, first descending, and keeps the sort in the URL", async () => {
  const { user, router } = await renderApp(MEMBERS);
  const grid = await screen.findByRole("grid", { name: "Members" });
  const header = screen.getByRole("columnheader", { name: "Role" });

  await user.click(within(header).getByRole("button"));

  await waitFor(() => expect(searchOf(router)).toMatchObject({ sort: "role", desc: true }));
  expect(
    within(grid)
      .getAllByText(/@example\.com$/)
      .map((node) => node.textContent),
  ).toEqual(["ada@example.com", "bob@example.com"]);
});

it("changes a member's role", async () => {
  let pathUser = "";
  let body: unknown;
  server.use(
    http.patch("/api/v1/orgs/:org/members/:user", async ({ params, request }) => {
      pathUser = String(params.user);
      body = await request.json();
      return HttpResponse.json({
        user: BOB,
        role: "admin",
        joined_at: "2026-01-02T09:00:00.000000000Z",
      });
    }),
  );

  const { user, queryClient, form } = await openMember("bob@example.com");
  // A role change also invalidates every cached project and project-member entry.
  const project = projectQuery("acme", "support-bot");
  const projectMembers = $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/members",
    projectMembersInit("acme", "support-bot"),
  );
  await queryClient.ensureQueryData(project);
  await queryClient.ensureQueryData(projectMembers);

  await changeRoleTo(user, form, "Admin");

  await waitFor(() => expect(body).toEqual({ role: "admin" }));
  expect(pathUser).toBe(BOB_ID);
  await waitFor(() => {
    expect(queryClient.getQueryState(project.queryKey)?.isInvalidated).toBe(true);
    expect(queryClient.getQueryState(projectMembers.queryKey)?.isInvalidated).toBe(true);
  });
});

it("explains the last-owner rule", async () => {
  server.use(
    http.patch("/api/v1/orgs/:org/members/:user", () =>
      problemResponse(409, "conflict", { detail: "an org needs an owner" }),
    ),
  );

  const { user, form } = await openMember("ada@example.com");
  await changeRoleTo(user, form, "Member");

  expect(
    await within(form).findByText(
      "An organization needs at least one owner. Make someone else an owner first.",
    ),
  ).toBeInTheDocument();
});

it("explains a seat limit on a role change and offers an upgrade", async () => {
  server.use(
    http.patch("/api/v1/orgs/:org/members/:user", () =>
      problemResponse(403, "plan_limit", { detail: "Your plan allows 1 seat." }),
    ),
  );

  const { user, form } = await openMember("bob@example.com");
  await changeRoleTo(user, form, "Admin");

  expect(await within(form).findByText("Your plan allows 1 seat.")).toBeInTheDocument();
  expect(await within(form).findByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
  expect(screen.queryByText("You can't give that role.")).toBeNull();
});

it("explains a role above the caller's", async () => {
  server.use(
    http.patch("/api/v1/orgs/:org/members/:user", () => problemResponse(403, "forbidden")),
  );

  const { user, form } = await openMember("bob@example.com");
  await changeRoleTo(user, form, "Viewer");

  expect(await within(form).findByText("You can't give that role.")).toBeInTheDocument();
});

it("offers only roles up to the caller's", async () => {
  server.use(meAs("admin"));

  const { user, form } = await openMember("bob@example.com");
  await user.click(within(form).getByRole("combobox"));
  const names = (await screen.findAllByRole("option")).map((option) => option.textContent);
  expect(names).toEqual(["Admin", "Member", "Viewer"]);
  await user.keyboard("{Escape}");
});

it("disables a role an admin cannot change, and says why", async () => {
  server.use(meAs("admin"));

  const { form } = await openMember("ada@example.com");

  const role = within(form).getByRole("combobox");
  expect(role).toBeDisabled();
  expect(role.parentElement).toHaveAccessibleDescription(
    "Only an owner can change an owner's role",
  );
});

it("removes a member after typing their email", async () => {
  const users: string[] = [];
  server.use(
    http.delete("/api/v1/orgs/:org/members/:user", ({ params }) => {
      users.push(String(params.user));
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user, router } = await openMember("bob@example.com");
  await user.click(screen.getByRole("button", { name: "Remove…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Remove member?" });
  const confirm = within(dialog).getByRole("button", { name: "Remove" });
  expect(confirm).toBeDisabled();
  await user.type(within(dialog).getByRole("textbox"), "bob@example.com");
  await user.click(confirm);

  expect(users).toEqual([BOB_ID]);
  await waitFor(() => expect(searchOf(router).open).toBeUndefined());
});

it("keeps the dialog open when the last owner tries to leave", async () => {
  server.use(
    http.delete("/api/v1/orgs/:org/members/:user", () =>
      problemResponse(409, "conflict", { detail: "an org needs an owner" }),
    ),
  );

  const { user } = await openMember("ada@example.com");
  await user.click(screen.getByRole("button", { name: "Leave…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Leave organization?" });
  await user.type(within(dialog).getByRole("textbox"), "ada@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Leave" }));

  const open = await screen.findByRole("alertdialog", { name: "Leave organization?" });
  expect(within(open).getByRole("alert")).toHaveTextContent(
    "An organization needs at least one owner. Make someone else an owner first.",
  );
});

it("lets a member leave and goes home", async () => {
  let orgs = [{ id: ORG_ID, slug: "acme", name: "Acme", role: "member" }];
  let invitesCalls = 0;
  server.use(
    http.get("/api/auth/me", () =>
      HttpResponse.json({ user: { ...ADA, is_instance_admin: true }, orgs }),
    ),
    http.get("/api/v1/orgs/:org/invites", () => {
      invitesCalls += 1;
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
    http.delete("/api/v1/orgs/:org/members/:user", () => {
      orgs = [];
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { router, user } = await openMember("ada@example.com");
  await user.click(screen.getByRole("button", { name: "Leave…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Leave organization?" });
  await user.type(within(dialog).getByRole("textbox"), "ada@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Leave" }));

  await waitFor(() => expect(router.state.location.pathname).toBe("/"));
  expect(invitesCalls).toBe(0);
});

it("disables actions a viewer cannot take", async () => {
  let invitesCalls = 0;
  server.use(
    meAs("viewer"),
    http.get("/api/v1/orgs/:org/invites", () => {
      invitesCalls += 1;
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );

  const { form } = await openMember("bob@example.com");

  const role = within(form).getByRole("combobox");
  expect(role).toBeDisabled();
  expect(role.parentElement).toHaveAccessibleDescription("Only admins and owners can change roles");
  const remove = screen.getByRole("button", { name: "Remove…" });
  expect(remove).toBeDisabled();
  expect(remove.parentElement).toHaveAccessibleDescription(
    "Only admins and owners can remove members",
  );
  expect(screen.queryByRole("tab", { name: "Invites" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Invite member" })).toBeNull();
  expect(invitesCalls).toBe(0);
});

it("loads more members", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/members", ({ request }) => {
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
              role: "viewer",
              joined_at: "2026-01-03T09:00:00.000000000Z",
            },
          ],
          next_cursor: null,
        });
      }
      return HttpResponse.json({
        data: [{ user: ADA, role: "owner", joined_at: "2026-01-01T09:00:00.000000000Z" }],
        next_cursor: "c2",
      });
    }),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Load more" }));

  const grid = await screen.findByRole("grid", { name: "Members" });
  expect(await within(grid).findByText("carol@example.com")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
});

it("asks before discarding an unsaved role change", async () => {
  const { user, form, router } = await openMember("bob@example.com");
  await user.click(within(form).getByRole("combobox"));
  await user.click(await screen.findByRole("option", { name: "Admin" }));

  await user.click(screen.getByRole("button", { name: "Close panel" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  expect(searchOf(router)).toMatchObject({ open: BOB_ID });

  await user.click(screen.getByRole("button", { name: "Close panel" }));
  const again = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(again).getByRole("button", { name: "Discard" }));
  await waitFor(() => expect(searchOf(router).open).toBeUndefined());
});

it("invites a member and shows the link without SMTP", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/invites", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(inviteCreated, { status: 201 });
    }),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  await waitFor(() => expect(body).toEqual({ email: "guest@example.com", role: "member" }));
  expect(await screen.findByText(/^Email is not configured on this server/)).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Invitation link" })).toHaveValue(
    "http://localhost:8080/invite/EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXA",
  );
  await user.click(screen.getByRole("button", { name: "Done" }));
  expect(screen.queryByRole("dialog")).toBeNull();
});

it("invites with another role", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/invites", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(inviteCreated, { status: 201 });
    }),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("combobox", { name: "Role" }));
  await user.click(await screen.findByRole("option", { name: "Admin" }));
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  await waitFor(() => expect(body).toEqual({ email: "guest@example.com", role: "admin" }));
});

it("says the invitation was emailed when SMTP is on", async () => {
  server.use(
    http.get("/api/auth/config", () =>
      HttpResponse.json({ ...authConfigFixture, smtp_enabled: true }),
    ),
    http.post("/api/v1/orgs/:org/invites", () => HttpResponse.json(inviteCreated, { status: 201 })),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  expect(await screen.findByText(/^We emailed an invitation to/)).toBeInTheDocument();
});

it("shows an existing member", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/invites", () =>
      problemResponse(409, "conflict", { detail: "already a member" }),
    ),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  expect(await screen.findByText("This person is already a member.")).toBeInTheDocument();
  expect(within(dialog).getByLabelText("Email")).toHaveAttribute("aria-invalid", "true");
});

it("shows the server's email error", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/invites", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "email", message: "invalid email" }],
      }),
    ),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "nope");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  expect(await screen.findByText("invalid email")).toBeInTheDocument();
});

it("explains a seat limit on invite and offers an upgrade", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/invites", () =>
      problemResponse(403, "plan_limit", { detail: "Your plan allows 1 seat." }),
    ),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  expect(await within(dialog).findByText("Your plan allows 1 seat.")).toBeInTheDocument();
  expect(await within(dialog).findByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
  expect(within(dialog).queryByText("You can't invite with that role.")).toBeNull();
});

it("drops the link when the dialog closes", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/invites", () => HttpResponse.json(inviteCreated, { status: 201 })),
  );

  const { user, queryClient } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));
  await screen.findByRole("dialog", { name: "Invitation created" });
  await user.keyboard("{Escape}");

  expect(screen.queryByRole("dialog")).toBeNull();
  await waitFor(() =>
    expect(
      JSON.stringify(
        queryClient
          .getMutationCache()
          .getAll()
          .map((mutation) => mutation.state.data ?? null),
      ),
    ).not.toContain("EXAMPLE0"),
  );

  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const reopened = await screen.findByRole("dialog", { name: "Invite member" });
  expect(within(reopened).getByLabelText("Email")).toHaveValue("");
  expect(screen.queryByRole("textbox", { name: "Invitation link" })).toBeNull();
});

it("keeps the invite dialog open while the request runs", async () => {
  let resolveInvite: (response: Response) => void = () => {};
  server.use(
    http.post("/api/v1/orgs/:org/invites", async () => {
      return await new Promise<Response>((resolve) => {
        resolveInvite = resolve;
      });
    }),
  );

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  await user.keyboard("{Escape}");
  expect(screen.getByRole("dialog", { name: "Invite member" })).toBeInTheDocument();

  await act(async () => {
    resolveInvite(HttpResponse.json(inviteCreated, { status: 201 }));
  });

  const result = await screen.findByRole("dialog", { name: "Invitation created" });
  expect(within(result).getByRole("textbox", { name: "Invitation link" })).toHaveValue(
    "http://localhost:8080/invite/EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXA",
  );
});

it("revokes an invitation after typing its email", async () => {
  const ids: string[] = [];
  server.use(
    http.delete("/api/v1/orgs/:org/invites/:id", ({ params }) => {
      ids.push(String(params.id));
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user, router } = await renderApp(`${MEMBERS}?tab=invites`);
  const invites = await screen.findByRole("grid", { name: "Pending invitations" });
  await user.click(within(invites).getByText("guest@example.com"));
  await screen.findByRole("button", { name: "Revoke…" });
  await user.click(screen.getByRole("button", { name: "Revoke…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Revoke invitation?" });
  await user.type(within(dialog).getByRole("textbox"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Revoke" }));

  expect(ids).toEqual([INVITE_ID]);
  await waitFor(() => expect(searchOf(router).open).toBeUndefined());
});

it("shows a failed removal in the dialog", async () => {
  server.use(
    http.delete("/api/v1/orgs/:org/members/:user", () => problemResponse(500, "internal")),
  );

  const { user } = await openMember("bob@example.com");
  await user.click(screen.getByRole("button", { name: "Remove…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Remove member?" });
  await user.type(within(dialog).getByRole("textbox"), "bob@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Remove" }));

  const open = await screen.findByRole("alertdialog", { name: "Remove member?" });
  expect(within(open).getByRole("alert")).toHaveTextContent("Internal Server Error");
});

it("shows a failed revoke in the dialog", async () => {
  server.use(http.delete("/api/v1/orgs/:org/invites/:id", () => HttpResponse.error()));

  const { user } = await renderApp(`${MEMBERS}?tab=invites&open=${INVITE_ID}`);
  await user.click(await screen.findByRole("button", { name: "Revoke…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Revoke invitation?" });
  await user.type(within(dialog).getByRole("textbox"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Revoke" }));

  const open = await screen.findByRole("alertdialog", { name: "Revoke invitation?" });
  expect(within(open).getByRole("alert")).toHaveTextContent("Could not revoke the invitation.");
});

it("explains an invitation above the caller's role", async () => {
  server.use(http.post("/api/v1/orgs/:org/invites", () => problemResponse(403, "forbidden")));

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.type(within(dialog).getByLabelText("Email"), "guest@example.com");
  await user.click(within(dialog).getByRole("button", { name: "Send invitation" }));

  expect(await within(dialog).findByRole("alert")).toHaveTextContent(
    "You can't invite with that role.",
  );
});

it("offers an admin only roles up to admin when inviting", async () => {
  server.use(meAs("admin"));

  const { user } = await renderApp(MEMBERS);
  await screen.findByRole("grid", { name: "Members" });
  await user.click(screen.getByRole("button", { name: "Invite member" }));
  const dialog = await screen.findByRole("dialog", { name: "Invite member" });
  await user.click(within(dialog).getByRole("combobox", { name: "Role" }));

  const names = (await screen.findAllByRole("option")).map((option) => option.textContent);
  expect(names).toEqual(["Admin", "Member", "Viewer"]);
});
