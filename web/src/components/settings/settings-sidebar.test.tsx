import { screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { renderApp } from "@/test/render";

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
