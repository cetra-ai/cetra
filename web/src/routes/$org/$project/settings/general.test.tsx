import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { meQuery, projectQuery, traceDetailQuery } from "@/api/queries";
import { server } from "@/mocks/node";
import { ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import deletionProjectFixture from "../../../../../../fixtures/api/static/deletion-project.json";
import meFixture from "../../../../../../fixtures/api/static/me.json";
import entitlementsFixture from "../../../../../../fixtures/api/static/org-entitlements.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const projectNav = () => within(screen.getByRole("list", { name: "Project settings" }));
const form = (name: string) => within(screen.getByRole("form", { name }));

/**
 * A project that keeps what it is saved with, as the server does: a save changes what the next
 * read returns, so the page's dirty state settles after the save.
 */
function savingProject(onPatch: (body: Record<string, unknown>) => void = () => {}) {
  let current: Record<string, unknown> = { ...projectFixture };
  return [
    http.get("/api/v1/projects/:project", () => HttpResponse.json(current)),
    http.patch("/api/v1/projects/:project", async ({ request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      onPatch(body);
      current = { ...current, ...body };
      return HttpResponse.json(current);
    }),
  ];
}

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

it("replaces the project sidebar with the settings sidebar", async () => {
  await renderApp("/acme/support-bot/settings/general");

  await screen.findByRole("heading", { name: "General", level: 1 });
  expect(screen.getByRole("link", { name: "Back to project" })).toHaveAttribute(
    "href",
    "/acme/support-bot/traces",
  );
  expect(screen.getByRole("list", { name: "Project settings" })).toBeInTheDocument();
  expect(screen.getByRole("list", { name: "Organization settings" })).toBeInTheDocument();
  expect(projectNav().getByRole("link", { name: "General" })).toHaveAttribute(
    "aria-current",
    "page",
  );
  expect(screen.queryByRole("link", { name: "Traces" })).toBeNull();
});

it("opens settings from the project sidebar", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await user.click(screen.getByRole("link", { name: "Settings" }));

  expect(router.state.location.pathname).toBe("/acme/support-bot/settings/general");
});

it("shows Settings in the project sidebar for a viewer", async () => {
  server.use(projectAs("viewer"));

  await renderApp("/acme/support-bot/traces");

  expect(await screen.findByRole("link", { name: "Settings" })).toBeInTheDocument();
});

it("saves only the name from the Project card", async () => {
  let body: unknown;
  server.use(
    ...savingProject((sent) => {
      body = sent;
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Support");
  await user.click(form("Project").getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ name: "Support" }));
  expect(await form("Project").findByRole("status")).toHaveTextContent("Saved");
});

it("saves only the retention from the Data retention card", async () => {
  let body: unknown;
  server.use(
    ...savingProject((sent) => {
      body = sent;
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const days = await screen.findByLabelText("Retention");
  await user.clear(days);
  await user.type(days, "7");
  await user.click(form("Data retention").getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ retention_days: 7 }));
  expect(await form("Data retention").findByRole("status")).toHaveTextContent("Saved");
});

it("sends null to keep data forever", async () => {
  let body: unknown;
  server.use(
    ...savingProject((sent) => {
      body = sent;
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  await screen.findByLabelText("Name");
  await user.click(screen.getByRole("checkbox", { name: "Keep data forever" }));
  expect(screen.getByLabelText("Retention")).toBeDisabled();
  await user.click(form("Data retention").getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ retention_days: null }));
});

it("shows an invalid retention as you type and keeps Save disabled", async () => {
  let calls = 0;
  server.use(
    http.patch("/api/v1/projects/:project", () => {
      calls += 1;
      return HttpResponse.json(projectFixture);
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const days = await screen.findByLabelText("Retention");
  await user.clear(days);
  await user.type(days, "0");

  expect(
    await screen.findByText("Enter a whole number of days from 1 to 36500."),
  ).toBeInTheDocument();
  expect(days).toHaveAttribute("aria-invalid", "true");
  expect(form("Data retention").getByRole("button", { name: "Save" })).toBeDisabled();
  expect(calls).toBe(0);
});

it("does not show an error for a field the user has not touched", async () => {
  await renderApp("/acme/support-bot/settings/general");

  await screen.findByLabelText("Retention");
  expect(screen.queryByText("Enter a name.")).toBeNull();
  expect(screen.queryByText(/Enter a whole number of days/)).toBeNull();
});

it("shows a refusal from the server", async () => {
  server.use(http.patch("/api/v1/projects/:project", () => problemResponse(403, "forbidden")));

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const name = await screen.findByLabelText("Name");
  await user.type(name, " bot");
  await user.click(form("Project").getByRole("button", { name: "Save" }));

  expect(await form("Project").findByRole("alert")).toHaveTextContent(
    "Only project admins can change these settings.",
  );
});

it("shows the server's field errors under their fields", async () => {
  server.use(
    http.patch("/api/v1/projects/:project", () =>
      problemResponse(400, "bad_request", {
        errors: [
          { field: "name", message: "must be 1 to 100 characters" },
          { field: "retention_days", message: "must be 1 to 36500" },
        ],
      }),
    ),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const name = await screen.findByLabelText("Name");
  await user.type(name, " bot");
  await user.click(form("Project").getByRole("button", { name: "Save" }));

  expect(await screen.findByText("must be 1 to 100 characters")).toBeInTheDocument();
  expect(screen.getByLabelText("Name")).toHaveAttribute("aria-invalid", "true");

  const days = screen.getByLabelText("Retention");
  await user.clear(days);
  await user.type(days, "7");
  await user.click(form("Data retention").getByRole("button", { name: "Save" }));

  expect(await screen.findByText("must be 1 to 36500")).toBeInTheDocument();
  expect(screen.getByLabelText("Retention")).toHaveAttribute("aria-invalid", "true");
});

it("is read-only for a member", async () => {
  server.use(projectAs("member"));

  await renderApp("/acme/support-bot/settings/general");

  expect(await screen.findByLabelText("Name")).toBeDisabled();
  const saves = screen.getAllByRole("button", { name: "Save" });
  expect(saves).toHaveLength(3);
  for (const save of saves) {
    expect(save).toBeDisabled();
  }
  expect(saves[0]?.parentElement).toHaveAccessibleDescription(
    "Only project admins can change these settings",
  );
});

it("asks before leaving with unsaved edits, and stays when asked", async () => {
  const { router, user } = await renderApp("/acme/support-bot/settings/general");
  const name = await screen.findByLabelText("Name");
  await user.type(name, " bot");

  await user.click(screen.getByRole("link", { name: "Back to project" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));

  expect(router.state.location.pathname).toBe("/acme/support-bot/settings/general");
  expect(screen.getByLabelText("Name")).toHaveValue("Support bot bot");

  await user.click(screen.getByRole("link", { name: "Back to project" }));
  await user.click(
    within(await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" })).getByRole(
      "button",
      { name: "Discard" },
    ),
  );

  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/traces"));
});

it("deletes the project once its slug is typed, then returns to the org home", async () => {
  let path = "";
  let confirm: string | null = null;
  server.use(
    http.delete("/api/v1/projects/:project", ({ request }) => {
      const url = new URL(request.url);
      path = url.pathname;
      confirm = url.searchParams.get("confirm");
      return HttpResponse.json(deletionProjectFixture, { status: 202 });
    }),
  );

  const { router, user, queryClient } = await renderApp("/acme/support-bot/settings/general");
  const cachedTrace = traceDetailQuery("acme", "support-bot", "a".repeat(32)).queryKey;
  queryClient.setQueryData(cachedTrace, {});
  const open = await screen.findByRole("button", { name: "Delete project…" });
  await waitFor(() => expect(open).toBeEnabled());
  await user.click(open);

  const dialog = await screen.findByRole("alertdialog");
  const confirmButton = within(dialog).getByRole("button", { name: "Delete project" });
  expect(confirmButton).toBeDisabled();
  await user.type(
    within(dialog).getByLabelText('Type the project slug "support-bot" to confirm'),
    "support-bot",
  );
  await user.click(confirmButton);

  await waitFor(() => expect(router.state.location.pathname).toBe("/acme"));
  expect(path).toBe("/api/v1/projects/acme%2Fsupport-bot");
  expect(confirm).toBe("support-bot");
  await waitFor(() => expect(queryClient.getQueryState(cachedTrace)).toBeUndefined());
});

it("disables project deletion for a project admin who is not an org admin", async () => {
  server.use(
    projectAs("admin"),
    http.get("/api/auth/me", () =>
      HttpResponse.json({
        user: meFixture.user,
        orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role: "member" }],
      }),
    ),
  );

  const { queryClient } = await renderApp("/acme/support-bot/settings/general");
  await waitFor(() => expect(queryClient.getQueryData(meQuery.queryKey)).toBeDefined());

  const button = screen.getByRole("button", { name: "Delete project…" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription(
    "Only organization admins and owners can delete projects",
  );
});

it("shows the org-admin message when the server refuses the deletion", async () => {
  server.use(http.delete("/api/v1/projects/:project", () => problemResponse(403, "forbidden")));

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const open = await screen.findByRole("button", { name: "Delete project…" });
  await waitFor(() => expect(open).toBeEnabled());
  await user.click(open);

  const dialog = await screen.findByRole("alertdialog");
  await user.type(
    within(dialog).getByLabelText('Type the project slug "support-bot" to confirm'),
    "support-bot",
  );
  await user.click(within(dialog).getByRole("button", { name: "Delete project" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Only organization admins and owners can delete projects.",
  );
});

it("saves the trace completion timeout", async () => {
  let body: unknown;
  server.use(
    ...savingProject((sent) => {
      body = sent;
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const timeout = await screen.findByLabelText("Trace completion timeout");
  await user.clear(timeout);
  await user.type(timeout, "120");
  await user.click(form("Online evaluation").getByRole("button", { name: "Save" }));

  await waitFor(() => expect(body).toEqual({ trace_completion_timeout_s: 120 }));
  expect(await form("Online evaluation").findByRole("status")).toHaveTextContent("Saved");
});

it("re-syncs the timeout field when the saved value changes", async () => {
  let savedTimeout = 60;
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, trace_completion_timeout_s: savedTimeout }),
    ),
  );

  const { queryClient } = await renderApp("/acme/support-bot/settings/general");
  const timeout = await screen.findByLabelText("Trace completion timeout");
  expect(timeout).toHaveValue(60);

  savedTimeout = 90;
  await queryClient.invalidateQueries({ queryKey: projectQuery("acme", "support-bot").queryKey });

  await waitFor(() => expect(timeout).toHaveValue(90));
});

it("keeps a timeout the user has typed when the saved value changes", async () => {
  let savedTimeout = 60;
  let reads = 0;
  server.use(
    http.get("/api/v1/projects/:project", () => {
      reads += 1;
      return HttpResponse.json({ ...projectFixture, trace_completion_timeout_s: savedTimeout });
    }),
  );

  const { user, queryClient } = await renderApp("/acme/support-bot/settings/general");
  const timeout = await screen.findByLabelText("Trace completion timeout");
  await user.clear(timeout);
  await user.type(timeout, "120");

  savedTimeout = 90;
  const readsBefore = reads;
  const key = projectQuery("acme", "support-bot").queryKey;
  await queryClient.invalidateQueries({ queryKey: key });
  await waitFor(() => expect(reads).toBeGreaterThan(readsBefore));
  await waitFor(() =>
    expect(queryClient.getQueryData(key)).toMatchObject({ trace_completion_timeout_s: 90 }),
  );

  expect(timeout).toHaveValue(120);
});

it("shows an out-of-range timeout as you type and does not call the server", async () => {
  let calls = 0;
  server.use(
    http.patch("/api/v1/projects/:project", () => {
      calls += 1;
      return HttpResponse.json(projectFixture);
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const timeout = await screen.findByLabelText("Trace completion timeout");
  await user.clear(timeout);
  await user.type(timeout, "5");

  expect(await screen.findByText("Enter a whole number from 10 to 3600.")).toBeInTheDocument();
  expect(form("Online evaluation").getByRole("button", { name: "Save" })).toBeDisabled();
  expect(calls).toBe(0);
});

it("shows a server field error under the timeout", async () => {
  server.use(
    http.patch("/api/v1/projects/:project", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "trace_completion_timeout_s", message: "must be 10 to 3600" }],
      }),
    ),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const timeout = await screen.findByLabelText("Trace completion timeout");
  await user.clear(timeout);
  await user.type(timeout, "15");
  await user.click(form("Online evaluation").getByRole("button", { name: "Save" }));

  expect(await screen.findByText("must be 10 to 3600")).toBeInTheDocument();
  expect(screen.getByLabelText("Trace completion timeout")).toHaveAttribute("aria-invalid", "true");
});

it("does not let a member change the timeout", async () => {
  server.use(projectAs("member"));

  await renderApp("/acme/support-bot/settings/general");

  expect(await screen.findByLabelText("Trace completion timeout")).toBeDisabled();
  expect(form("Online evaluation").getByRole("button", { name: "Save" })).toBeDisabled();
});

it("shows the Pro plan message and an Upgrade link when retention is refused by the plan", async () => {
  server.use(
    http.patch("/api/v1/projects/:project", () =>
      problemResponse(403, "plan_required", {
        detail: "data retention management is part of the Pro plan",
      }),
    ),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  const days = await screen.findByLabelText("Retention");
  await user.clear(days);
  await user.type(days, "7");
  await user.click(form("Data retention").getByRole("button", { name: "Save" }));

  expect(await form("Data retention").findByRole("alert")).toHaveTextContent(
    "Data retention management is part of the Pro plan.",
  );
  expect(await screen.findByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});

it("states the plan's data window under the retention field", async () => {
  await renderApp("/acme/support-bot/settings/general");

  const days = await screen.findByLabelText("Retention");
  await waitFor(() =>
    expect(days).toHaveAccessibleDescription(/Your plan shows the last 1825 days/),
  );
  expect(screen.queryByRole("note", { name: "Locked feature" })).toBeNull();
});

it("locks the retention days without retention_management and keeps Keep data forever", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({ ...entitlementsFixture, features: ["project_roles"] }),
    ),
  );

  await renderApp("/acme/support-bot/settings/general");

  const lock = await screen.findByRole("note", { name: "Locked feature" });
  expect(lock).toHaveTextContent("Available on the Pro plan.");
  expect(within(lock).getByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
  expect(screen.getByLabelText("Retention")).toBeDisabled();
  expect(screen.getByRole("checkbox", { name: "Keep data forever" })).toBeEnabled();
});

it("renames a project without sending retention when it is locked and already set", async () => {
  let body: unknown;
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () =>
      HttpResponse.json({ ...entitlementsFixture, features: ["project_roles"] }),
    ),
    ...savingProject((sent) => {
      body = sent;
    }),
  );

  const { user } = await renderApp("/acme/support-bot/settings/general");
  await screen.findByRole("note", { name: "Locked feature" });
  const name = screen.getByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Support");
  await user.click(form("Project").getByRole("button", { name: "Save" }));

  expect(await form("Project").findByRole("status")).toHaveTextContent("Saved");
  expect(body).toEqual({ name: "Support" });
});
