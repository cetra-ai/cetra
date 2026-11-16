import { screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { renderApp } from "@/test/render";

const DESKTOP_WIDTH = window.innerWidth;

afterEach(() => {
  window.innerWidth = DESKTOP_WIDTH;
});

it("opens the navigation from the page on a narrow screen", async () => {
  window.innerWidth = 375;
  const { user } = await renderApp("/acme/support-bot/traces");
  const triggers = await screen.findAllByRole("button", { name: "Toggle Sidebar" });
  expect(screen.queryByRole("link", { name: "Dashboard" })).not.toBeInTheDocument();
  await user.click(triggers[0] as HTMLElement);
  expect(await screen.findByRole("link", { name: "Dashboard" })).toBeInTheDocument();
});
