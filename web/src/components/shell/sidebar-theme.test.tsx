import { act, screen, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { PALETTE_STORAGE_KEY } from "@/components/theme-provider";
import { renderApp } from "@/test/render";

/** The project sidebar's footer, around Settings. */
async function footerButtons() {
  const settings = await screen.findByRole("link", { name: "Settings" });
  const footer = settings.closest<HTMLElement>('[data-slot="sidebar-footer"]');
  if (!footer) throw new Error("the sidebar footer is missing");
  return footer;
}

it("puts Theme just above Settings in the sidebar footer", async () => {
  await renderApp("/acme/support-bot/traces");
  const footer = await footerButtons();
  const theme = within(footer).getByRole("button", { name: "Theme" });
  const settings = within(footer).getByRole("link", { name: "Settings" });
  expect(theme.compareDocumentPosition(settings) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
});

it("offers the theme, palette and density, and sets the theme", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await user.click(within(await footerButtons()).getByRole("button", { name: "Theme" }));
  for (const name of ["Light", "Dark", "System", "Nord", "Comfortable"]) {
    expect(screen.getByRole("menuitemradio", { name })).toBeInTheDocument();
  }
  await user.click(screen.getByRole("menuitemradio", { name: "Dark" }));
  expect(document.documentElement).toHaveClass("dark");
});

it("restores the saved palette when the menu closes without a pick", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await user.click(within(await footerButtons()).getByRole("button", { name: "Theme" }));
  // Radix moves focus to an item on hover: focusing it here previews the palette.
  act(() => {
    screen.getByRole("menuitemradio", { name: "Tokyo Night" }).focus();
  });
  expect(document.documentElement.dataset.palette).toBe("tokyo");

  await user.keyboard("{Escape}");

  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  expect(document.documentElement.dataset.palette).toBeUndefined();
  expect(localStorage.getItem(PALETTE_STORAGE_KEY)).toBeNull();
});
