import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Suspense } from "react";
import { describe, expect, it } from "vitest";
import { SidebarAccount } from "@/components/shell/sidebar-account";
import { PALETTE_STORAGE_KEY, ThemeProvider } from "@/components/theme-provider";
import { Sidebar, SidebarFooter, SidebarProvider } from "@/components/ui/sidebar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { DOCS_URL } from "@/product";

/** Renders the account footer in a bare sidebar. A root-only router provides the links. */
async function renderAccount({ defaultOpen = true }: { defaultOpen?: boolean } = {}) {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: Infinity, staleTime: Infinity },
    },
  });
  const rootRoute = createRootRoute({
    component: () => (
      <TooltipProvider>
        <SidebarProvider defaultOpen={defaultOpen}>
          <Sidebar collapsible="icon">
            <SidebarFooter>
              <Suspense fallback={null}>
                <SidebarAccount />
              </Suspense>
            </SidebarFooter>
          </Sidebar>
        </SidebarProvider>
      </TooltipProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/acme/support-bot/traces"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <RouterProvider router={router} />
      </ThemeProvider>
    </QueryClientProvider>,
  );
  await router.load();
  return { user: userEvent.setup() };
}

describe("SidebarAccount", () => {
  it("shows the name and the email", async () => {
    await renderAccount();

    expect(await screen.findByRole("button", { name: "Ada Example" })).toBeInTheDocument();
    expect(screen.getByText("ada@example.com")).toBeInTheDocument();
  });

  it("opens the account menu with the account links, the theme submenu and sign out", async () => {
    const { user } = await renderAccount();

    await user.click(await screen.findByRole("button", { name: "Ada Example" }));
    expect(screen.getByRole("menuitem", { name: "Account" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Admin" })).toBeInTheDocument();
    expect(screen.getByRole("menuitemcheckbox", { name: "Show times in UTC" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Sign out" })).toBeInTheDocument();

    await user.click(screen.getByRole("menuitem", { name: "Theme" }));
    expect(screen.getByRole("menuitemradio", { name: "Light" })).toBeInTheDocument();
    expect(screen.getByRole("menuitemradio", { name: "Dark" })).toBeInTheDocument();
    expect(screen.getByRole("menuitemradio", { name: "System" })).toBeInTheDocument();
    expect(screen.getByRole("menuitemradio", { name: "Nord" })).toBeInTheDocument();
    expect(screen.getByRole("menuitemradio", { name: "Comfortable" })).toBeInTheDocument();
  });

  it("sets the theme from the theme submenu", async () => {
    const { user } = await renderAccount();

    await user.click(await screen.findByRole("button", { name: "Ada Example" }));
    await user.click(screen.getByRole("menuitem", { name: "Theme" }));
    await user.click(screen.getByRole("menuitemradio", { name: "Dark" }));

    expect(document.documentElement).toHaveClass("dark");
  });

  it("restores the saved palette when the whole menu closes", async () => {
    const { user } = await renderAccount();

    await user.click(await screen.findByRole("button", { name: "Ada Example" }));
    await user.click(screen.getByRole("menuitem", { name: "Theme" }));
    // Radix moves focus to an item on hover: focusing it here previews the palette.
    act(() => {
      screen.getByRole("menuitemradio", { name: "Tokyo Night" }).focus();
    });
    expect(document.documentElement.dataset.palette).toBe("tokyo");

    // The first Escape closes the submenu, the second closes the menu.
    await user.keyboard("{Escape}{Escape}");

    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(document.documentElement.dataset.palette).toBeUndefined();
    expect(localStorage.getItem(PALETTE_STORAGE_KEY)).toBeNull();
  });

  it("shows the version and a docs link", async () => {
    await renderAccount();

    expect(await screen.findByText("v0.0.0")).toBeInTheDocument();
    const docs = screen.getByRole("link", { name: /Docs/ });
    expect(docs).toHaveAttribute("href", DOCS_URL);
    expect(docs).toHaveAttribute("target", "_blank");
    expect(docs).toHaveAttribute("rel", "noreferrer");
  });

  it("shows only the docs icon when the sidebar is collapsed", async () => {
    await renderAccount({ defaultOpen: false });

    expect(await screen.findByRole("link", { name: "Docs" })).toHaveAttribute("href", DOCS_URL);
    expect(screen.queryByText("v0.0.0")).not.toBeInTheDocument();
  });
});
