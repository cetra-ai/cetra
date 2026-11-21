import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { ORG_ID, PROJECT_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import meFixture from "../../../fixtures/api/static/me.json";

const CREATED_AT = "2026-11-21T10:00:00.000000000Z";
const BETA_ORG_ID = "01900000-0000-7000-8000-000000000011";
const NUMBERS_ID = "01900000-0000-7000-8000-000000000005";
const DOCS_ID = "01900000-0000-7000-8000-000000000003";
const NEW_PROJECT_ID = "01900000-0000-7000-8000-000000000020";

type Role = "owner" | "admin" | "member" | "viewer";

function org(id: string, slug: string, name: string, role: Role) {
  return { id, slug, name, role, demo: false };
}

function project(id: string, orgId: string, slug: string, name: string, role: Role) {
  return {
    id,
    org_id: orgId,
    slug,
    name,
    trace_completion_timeout_s: 60,
    role,
    created_at: CREATED_AT,
  };
}

/** The signed-in user with the given orgs. */
function stubMe(orgs: ReturnType<typeof org>[]) {
  server.use(http.get("/api/auth/me", () => HttpResponse.json({ user: meFixture.user, orgs })));
}

/** The code's request: what the approval page shows. */
function stubCode(purpose: "login" | "init", host?: string) {
  const seen: string[] = [];
  server.use(
    http.get("/api/auth/device/:userCode", ({ params }) => {
      seen.push(String(params.userCode));
      return HttpResponse.json({ purpose, host, created_at: CREATED_AT });
    }),
  );
  return seen;
}

/** Projects per org slug; an org without an entry lists none. */
function stubProjects(byOrg: Record<string, ReturnType<typeof project>[]>) {
  server.use(
    http.get("/api/v1/orgs/:org/projects", ({ params }) =>
      HttpResponse.json({ data: byOrg[String(params.org)] ?? [], next_cursor: null }),
    ),
  );
}

/** Records the approve or deny calls: the path, and the JSON body. */
function stubDecision(kind: "approve" | "deny") {
  const calls: { path: string; body: unknown }[] = [];
  server.use(
    http.post(`/api/auth/device/:userCode/${kind}`, async ({ request, params }) => {
      const body = await request.text();
      calls.push({
        path: `${String(params.userCode)}/${kind}`,
        body: body === "" ? undefined : JSON.parse(body),
      });
      return new HttpResponse(null, { status: 204 });
    }),
  );
  return calls;
}

// The two-org setup of the picker tests: Acme (owner) lists Support bot (admin), Numbers
// (admin) and Playground (member); Beta (admin) lists Docs (owner) and Hidden (viewer).
function stubTwoOrgs() {
  stubMe([org(ORG_ID, "acme", "Acme", "owner"), org(BETA_ORG_ID, "beta", "Beta", "admin")]);
  stubProjects({
    acme: [
      project(PROJECT_ID, ORG_ID, "support-bot", "Support bot", "admin"),
      project(NUMBERS_ID, ORG_ID, "123", "Numbers", "admin"),
      project("01900000-0000-7000-8000-000000000002", ORG_ID, "playground", "Playground", "member"),
    ],
    beta: [
      project(DOCS_ID, BETA_ORG_ID, "docs", "Docs", "owner"),
      project("01900000-0000-7000-8000-000000000004", BETA_ORG_ID, "hidden", "Hidden", "viewer"),
    ],
  });
}

it("reads the code and the project from the raw query string", async () => {
  stubTwoOrgs();
  const seen = stubCode("init", "laptop");

  // The raw string `acme/123` must reach the picker byte for byte, so it selects the project
  // whose slug is `123`.
  await renderApp("/device?code=1234&project=acme/123");

  expect(await screen.findByRole("radio", { name: "Acme / Numbers" })).toBeChecked();
  expect(screen.getByText("1234")).toBeInTheDocument();
  expect(seen).toEqual(["1234"]);
});

it("sends a signed-out user through login and keeps the code", async () => {
  server.use(http.get("/api/auth/me", () => problemResponse(401, "unauthorized")));

  const { router } = await renderApp("/device?code=BCDF-GHJK");

  await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
  expect(router.state.location.search).toMatchObject({
    redirect: "/device?code=BCDF-GHJK",
  });
});

it("navigates to the code the user types, keeping the project", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubCode("login");

  const { router, user } = await renderApp("/device?project=acme%2Fsupport-bot");
  await user.type(await screen.findByLabelText("Code"), " bcdf-ghjk ");
  await user.click(screen.getByRole("button", { name: "Continue" }));

  await waitFor(() =>
    expect(router.state.location.search).toEqual({
      code: "bcdf-ghjk",
      project: "acme/support-bot",
    }),
  );
});

it("asks for a code when the field is empty", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);

  const { router, user } = await renderApp("/device");
  await user.click(await screen.findByRole("button", { name: "Continue" }));

  expect(await screen.findByText("Enter the code that the terminal shows.")).toBeInTheDocument();
  expect(router.state.location.search).toEqual({});
});

it("says so when the code is unknown or expired", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  server.use(http.get("/api/auth/device/:userCode", () => problemResponse(404, "not_found")));

  await renderApp("/device?code=BCDF-GHJK");

  expect(
    await screen.findByText(
      "This code is unknown or has expired. Run the command again to get a new code.",
    ),
  ).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Enter another code" })).toHaveAttribute(
    "href",
    "/device",
  );
  expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
});

it("shows the host, the warning and the read-and-write sentence for login", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubCode("login", "laptop");

  await renderApp("/device?code=BCDF-GHJK");

  expect(await screen.findByText("laptop")).toBeInTheDocument();
  expect(screen.getByRole("note")).toHaveTextContent(
    "Only continue if you just ran cetra-cli init, cetra-cli login or cetra login on this computer.",
  );
  expect(
    screen.getByText(
      "This creates a personal access token that can read and write, valid for 90 days.",
    ),
  ).toBeInTheDocument();
  // Login has no project to pick.
  expect(screen.queryByRole("radio")).toBeNull();
});

it("shows that the computer did not report a name", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubCode("login");

  await renderApp("/device?code=BCDF-GHJK");

  expect(await screen.findByText("not reported")).toBeInTheDocument();
});

it("approves a login code with an empty body and reports it", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubCode("login", "laptop");
  const approvals = stubDecision("approve");

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "Approve" }));

  expect(await screen.findByText("Done. Return to the terminal.")).toBeInTheDocument();
  expect(approvals).toEqual([{ path: "BCDF-GHJK/approve", body: {} }]);
});

it("denies a code and says it was cancelled", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubCode("login", "laptop");
  const denials = stubDecision("deny");

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "Deny" }));

  expect(await screen.findByText("Cancelled. You can close this window.")).toBeInTheDocument();
  expect(denials).toEqual([{ path: "BCDF-GHJK/deny", body: undefined }]);
});

it("lists only the projects the user may manage, and waits for a pick", async () => {
  stubTwoOrgs();
  stubCode("init", "laptop");
  const approvals = stubDecision("approve");

  const { user } = await renderApp("/device?code=BCDF-GHJK");

  expect(await screen.findByRole("radio", { name: "Acme / Support bot" })).toBeInTheDocument();
  expect(screen.getByRole("radio", { name: "Acme / Numbers" })).toBeInTheDocument();
  expect(screen.getByRole("radio", { name: "Beta / Docs" })).toBeInTheDocument();
  expect(screen.queryByRole("radio", { name: "Acme / Playground" })).toBeNull();
  expect(screen.queryByRole("radio", { name: "Beta / Hidden" })).toBeNull();

  const approve = screen.getByRole("button", { name: "Approve" });
  expect(approve).toBeDisabled();

  await user.click(screen.getByRole("radio", { name: "Beta / Docs" }));
  expect(approve).toBeEnabled();
  await user.click(approve);

  expect(await screen.findByText("Done. Return to the terminal.")).toBeInTheDocument();
  expect(approvals).toEqual([{ path: "BCDF-GHJK/approve", body: { project_id: DOCS_ID } }]);
});

it("preselects the project named by the project parameter", async () => {
  stubTwoOrgs();
  stubCode("init", "laptop");

  await renderApp("/device?code=BCDF-GHJK&project=acme/support-bot");

  expect(await screen.findByRole("radio", { name: "Acme / Support bot" })).toBeChecked();
  expect(screen.getByRole("button", { name: "Approve" })).toBeEnabled();
});

it("preselects the only eligible project", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "owner")]);
  stubProjects({
    acme: [
      project(PROJECT_ID, ORG_ID, "support-bot", "Support bot", "admin"),
      project("01900000-0000-7000-8000-000000000002", ORG_ID, "playground", "Playground", "member"),
    ],
  });
  stubCode("init", "laptop");

  await renderApp("/device?code=BCDF-GHJK");

  expect(await screen.findByRole("radio", { name: "Acme / Support bot" })).toBeChecked();
  expect(screen.getByRole("button", { name: "Approve" })).toBeEnabled();
});

it("says that an admin role is needed when the user manages no project", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubProjects({
    acme: [project(PROJECT_ID, ORG_ID, "support-bot", "Support bot", "member")],
  });
  stubCode("init", "laptop");

  await renderApp("/device?code=BCDF-GHJK");

  expect(
    await screen.findByText(
      "You need the admin role on a project to connect the CLI. Ask an admin of your organization.",
    ),
  ).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Approve" })).toBeDisabled();
  expect(screen.queryByRole("button", { name: "New project" })).toBeNull();
});

it("creates a project and selects it", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "owner")]);
  const projects: ReturnType<typeof project>[] = [];
  server.use(
    http.get("/api/v1/orgs/:org/projects", () =>
      HttpResponse.json({ data: projects, next_cursor: null }),
    ),
  );
  let created: unknown;
  server.use(
    http.post("/api/v1/orgs/:org/projects", async ({ request }) => {
      created = await request.json();
      const next = project(NEW_PROJECT_ID, ORG_ID, "edge", "Edge", "owner");
      projects.push(next);
      return HttpResponse.json(next, { status: 201 });
    }),
  );
  stubCode("init", "laptop");
  const approvals = stubDecision("approve");

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "New project" }));
  await user.type(screen.getByLabelText("Project name"), "Edge");
  await user.click(screen.getByRole("button", { name: "Create project" }));

  expect(await screen.findByRole("radio", { name: "Acme / Edge" })).toBeChecked();
  expect(created).toEqual({ name: "Edge" });
  await user.click(screen.getByRole("button", { name: "Approve" }));
  expect(await screen.findByText("Done. Return to the terminal.")).toBeInTheDocument();
  expect(approvals).toEqual([{ path: "BCDF-GHJK/approve", body: { project_id: NEW_PROJECT_ID } }]);
});

it("shows the plan message when the plan does not allow another project", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "owner")]);
  stubProjects({});
  server.use(
    http.post("/api/v1/orgs/:org/projects", () =>
      problemResponse(403, "plan_limit", { detail: "Your plan allows one project." }),
    ),
  );
  stubCode("init", "laptop");

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "New project" }));
  await user.type(screen.getByLabelText("Project name"), "Edge");
  await user.click(screen.getByRole("button", { name: "Create project" }));

  expect(await screen.findByRole("alert")).toHaveTextContent("Your plan allows one project.");
});

it("says to reload when the chosen project is not visible to approve", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubCode("login", "laptop");
  server.use(
    http.post("/api/auth/device/:userCode/approve", () => problemResponse(404, "not_found")),
  );

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "Approve" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Could not approve the code. Reload the page and choose a project.",
  );
  expect(screen.queryByText(/unknown or has expired/)).toBeNull();
  expect(screen.getByRole("button", { name: "Approve" })).toBeEnabled();
});

it("says that the project name is taken when creating a project gets a 409", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "owner")]);
  stubProjects({});
  server.use(http.post("/api/v1/orgs/:org/projects", () => problemResponse(409, "conflict")));
  stubCode("init", "laptop");

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "New project" }));
  await user.type(screen.getByLabelText("Project name"), "Edge");
  await user.click(screen.getByRole("button", { name: "Create project" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "A project with this name already exists. Choose another name.",
  );
});

it("says that only admins can create a project when creating gets a 403", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "owner")]);
  stubProjects({});
  server.use(http.post("/api/v1/orgs/:org/projects", () => problemResponse(403, "forbidden")));
  stubCode("init", "laptop");

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "New project" }));
  await user.type(screen.getByLabelText("Project name"), "Edge");
  await user.click(screen.getByRole("button", { name: "Create project" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Only admins and owners can create projects.",
  );
});

it("shows the problem when approving is refused", async () => {
  stubMe([org(ORG_ID, "acme", "Acme", "member")]);
  stubCode("login", "laptop");
  server.use(
    http.post("/api/auth/device/:userCode/approve", () =>
      problemResponse(403, "forbidden", { detail: "This code belongs to another account." }),
    ),
  );

  const { user } = await renderApp("/device?code=BCDF-GHJK");
  await user.click(await screen.findByRole("button", { name: "Approve" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "This code belongs to another account.",
  );
  expect(screen.getByRole("button", { name: "Approve" })).toBeEnabled();
});
