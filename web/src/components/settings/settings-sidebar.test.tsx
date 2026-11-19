import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, expect, it } from "vitest";
import { rememberLastProject } from "@/lib/last-project";
import { renderApp } from "@/test/render";

beforeEach(() => {
  localStorage.clear();
});

function filterBox() {
  return screen.getByRole("textbox", { name: "Filter settings" });
}

it("filters both groups by label, description and keywords", async () => {
  const { user } = await renderApp("/acme/support-bot/settings/general");
  const projectList = await screen.findByRole("list", { name: "Project settings" });
  expect(within(projectList).getByRole("link", { name: "API keys" })).toBeInTheDocument();

  await user.type(filterBox(), "retention");

  expect(within(projectList).getByRole("link", { name: "General" })).toBeInTheDocument();
  expect(within(projectList).queryByRole("link", { name: "API keys" })).toBeNull();
  expect(screen.queryByRole("list", { name: "Organization settings" })).toBeNull();
  expect(screen.queryByText("No settings match")).toBeNull();
});

it("matches the description of an organization page", async () => {
  const { user } = await renderApp("/acme/settings/general");
  const orgList = await screen.findByRole("list", { name: "Organization settings" });
  expect(within(orgList).getByRole("link", { name: "Single sign-on" })).toBeInTheDocument();

  await user.type(filterBox(), "identity provider");

  expect(within(orgList).getByRole("link", { name: "Single sign-on" })).toBeInTheDocument();
  expect(within(orgList).queryByRole("link", { name: "General" })).toBeNull();
  expect(screen.queryByRole("list", { name: "Project settings" })).toBeNull();
});

it("says so when nothing matches", async () => {
  const { user } = await renderApp("/acme/support-bot/settings/general");
  await screen.findByRole("list", { name: "Project settings" });

  await user.type(filterBox(), "no such setting");

  expect(screen.getByText("No settings match")).toBeInTheDocument();
  expect(screen.queryByRole("list", { name: "Project settings" })).toBeNull();
  expect(screen.queryByRole("list", { name: "Organization settings" })).toBeNull();
});

it("shows the usage meter in the footer and links it to the plan page", async () => {
  const { user, router } = await renderApp("/acme/support-bot/settings/general");
  const usage = await screen.findByRole("link", { name: /Usage/ });
  expect(within(usage).getByText("1,240 of 200,000 units · 1%")).toBeInTheDocument();

  await user.click(usage);

  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/settings/plan");
  });
});

it("lists only project items on a project page, with the project selected in the switch", async () => {
  await renderApp("/acme/support-bot/settings/general");

  expect(await screen.findByRole("list", { name: "Project settings" })).toBeInTheDocument();
  expect(screen.queryByRole("list", { name: "Organization settings" })).toBeNull();
  expect(screen.getByRole("tab", { name: "Support bot" })).toHaveAttribute("aria-selected", "true");
  expect(screen.getByRole("tab", { name: "Organization" })).toHaveAttribute(
    "aria-selected",
    "false",
  );
  expect(screen.getByRole("link", { name: "Back to Support bot" })).toHaveAttribute(
    "href",
    "/acme/support-bot/traces",
  );
});

it("navigates to the organization overview when Organization is chosen on a project page", async () => {
  const { user, router } = await renderApp("/acme/support-bot/settings/general");
  await screen.findByRole("list", { name: "Project settings" });

  await user.click(screen.getByRole("tab", { name: "Organization" }));

  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/settings");
  });
  await waitFor(() => expect(router.state.status).toBe("idle"));
  expect(await screen.findByRole("list", { name: "Organization settings" })).toBeInTheDocument();
  expect(screen.queryByRole("list", { name: "Project settings" })).toBeNull();
});

it("offers the remembered project on an organization page and navigates to its settings", async () => {
  rememberLastProject("acme", { slug: "support-bot", name: "Support bot" });
  const { user, router } = await renderApp("/acme/settings/general");

  expect(await screen.findByRole("list", { name: "Organization settings" })).toBeInTheDocument();
  expect(screen.getByRole("tab", { name: "Organization" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  expect(screen.getByRole("link", { name: "Back to Support bot" })).toHaveAttribute(
    "href",
    "/acme/support-bot/traces",
  );

  await user.click(screen.getByRole("tab", { name: "Support bot" }));

  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/settings");
  });
  await waitFor(() => expect(router.state.status).toBe("idle"));
  expect(await screen.findByRole("list", { name: "Project settings" })).toBeInTheDocument();
});

it("shows no project option and links back to the projects without a remembered project", async () => {
  await renderApp("/acme/settings/general");

  expect(await screen.findByRole("list", { name: "Organization settings" })).toBeInTheDocument();
  expect(screen.queryByRole("tab", { name: "Organization" })).toBeNull();
  expect(screen.getByText("Organization", { selector: "p" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Back to projects" })).toHaveAttribute("href", "/acme");
});

it("searches both scopes with a filter, under a label for each", async () => {
  rememberLastProject("acme", { slug: "support-bot", name: "Support bot" });
  const { user } = await renderApp("/acme/support-bot/settings/general");
  const projectList = await screen.findByRole("list", { name: "Project settings" });

  await user.type(filterBox(), "members");

  expect(within(projectList).getByRole("link", { name: "Members" })).toBeInTheDocument();
  const orgList = screen.getByRole("list", { name: "Organization settings" });
  expect(within(orgList).getByRole("link", { name: "Members" })).toBeInTheDocument();
  expect(
    screen.getByText("Project", { selector: "[data-slot='sidebar-group-label']" }),
  ).toBeInTheDocument();
  expect(
    screen.getByText("Organization", { selector: "[data-slot='sidebar-group-label']" }),
  ).toBeInTheDocument();
});
