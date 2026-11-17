import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it } from "vitest";
import { rememberItem } from "@/components/shell/recent-items";
import { server } from "@/mocks/node";
import { STORAGE_PREFIX } from "@/product";
import { renderApp } from "@/test/render";

const DESKTOP_WIDTH = window.innerWidth;
const SIDEBAR_KEY = `${STORAGE_PREFIX}sidebar`;
const RELEVANCE_ID = "01900000-0000-7000-8000-000000000500";
const ALERT_RULES = "/api/v1/projects/:project/alert-rules";

afterEach(() => {
  window.innerWidth = DESKTOP_WIDTH;
  localStorage.clear();
});

/** The desktop sidebar element (the one that carries data-state). */
function sidebar(): HTMLElement {
  return document.querySelector<HTMLElement>('[data-slot="sidebar"][data-state]') as HTMLElement;
}

it("opens the navigation from the page on a narrow screen", async () => {
  window.innerWidth = 375;
  const { user } = await renderApp("/acme/support-bot/traces");
  const triggers = await screen.findAllByRole("button", { name: "Toggle Sidebar" });
  expect(screen.queryByRole("link", { name: "Dashboard" })).not.toBeInTheDocument();
  await user.click(triggers[0] as HTMLElement);
  expect(await screen.findByRole("link", { name: "Dashboard" })).toBeInTheDocument();
});

it("groups the pages under their section headings", async () => {
  await renderApp("/acme/support-bot/traces");
  const nav = sidebar();
  for (const section of ["Observe", "Monitor", "Evaluate", "Explore"]) {
    expect(within(nav).getByText(section)).toBeInTheDocument();
  }
  const monitor = within(nav)
    .getByText("Monitor")
    .closest<HTMLElement>('[data-slot="sidebar-group"]');
  expect(monitor).not.toBeNull();
  expect(within(monitor as HTMLElement).getByRole("link", { name: "Alerts" })).toBeInTheDocument();
});

it("shows the firing count on Alerts and the paused dot on Online rules", async () => {
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByLabelText("1 alert rule firing")).toHaveTextContent("1");
  expect(await screen.findByLabelText("An online rule is paused on errors")).toBeInTheDocument();
});

it("shows no alerts badge when no rule is firing", async () => {
  server.use(http.get(ALERT_RULES, () => HttpResponse.json({ data: [] })));
  await renderApp("/acme/support-bot/traces");
  await screen.findByLabelText("An online rule is paused on errors");
  expect(screen.queryByLabelText(/alert rule.* firing/)).not.toBeInTheDocument();
});

it("shows the page's keys in its tooltip, in both modes", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await user.hover(screen.getByRole("link", { name: "Traces" }));
  const tooltip = await screen.findByRole("tooltip");
  expect(within(tooltip).getByText("G")).toBeInTheDocument();
  expect(within(tooltip).getByText("T")).toBeInTheDocument();
});

it("lists remembered items under Recent and opens them", async () => {
  rememberItem("acme", "support-bot", {
    kind: "evaluator",
    id: RELEVANCE_ID,
    label: "Relevance",
  });
  const { user, router } = await renderApp("/acme/support-bot/traces");
  const link = await screen.findByRole("link", { name: "Relevance" });
  expect(within(sidebar()).getByText("Recent")).toBeInTheDocument();
  await user.click(link);
  await waitFor(() => {
    expect(router.state.location.pathname).toBe(`/acme/support-bot/evaluators/${RELEVANCE_ID}`);
  });
});

it("shows no Recent group when nothing is remembered", async () => {
  await renderApp("/acme/support-bot/traces");
  expect(screen.queryByText("Recent")).not.toBeInTheDocument();
});

it("stores the collapsed choice when the sidebar is toggled", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  const triggers = await screen.findAllByRole("button", { name: "Toggle Sidebar" });
  await user.click(triggers[0] as HTMLElement);
  expect(localStorage.getItem(SIDEBAR_KEY)).toBe("closed");
  expect(sidebar()).toHaveAttribute("data-state", "collapsed");
});

it("renders collapsed when the stored choice is closed", async () => {
  localStorage.setItem(SIDEBAR_KEY, "closed");
  await renderApp("/acme/support-bot/traces");
  await screen.findAllByRole("button", { name: "Toggle Sidebar" });
  expect(sidebar()).toHaveAttribute("data-state", "collapsed");
});

it("starts collapsed on a narrow window and expanding does not store the choice", async () => {
  window.innerWidth = 900;
  const { user } = await renderApp("/acme/support-bot/traces");
  const triggers = await screen.findAllByRole("button", { name: "Toggle Sidebar" });
  expect(sidebar()).toHaveAttribute("data-state", "collapsed");
  await user.click(triggers[0] as HTMLElement);
  expect(sidebar()).toHaveAttribute("data-state", "expanded");
  expect(localStorage.getItem(SIDEBAR_KEY)).toBeNull();
});

it("shows the account and the settings link in the footer", async () => {
  await renderApp("/acme/support-bot/traces");
  expect(await screen.findByRole("button", { name: "Ada Example" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Settings" })).toBeInTheDocument();
});
