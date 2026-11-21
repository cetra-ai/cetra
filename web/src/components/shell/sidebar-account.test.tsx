import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Suspense } from "react";
import { describe, expect, it } from "vitest";
import { SidebarAccount } from "@/components/shell/sidebar-account";
import { ThemeProvider } from "@/components/theme-provider";
import { Sidebar, SidebarFooter, SidebarProvider } from "@/components/ui/sidebar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { APP_VERSION, DOCS_URL } from "@/product";

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

  it("opens the account menu with the account links, the time zone and sign out", async () => {
    const { user } = await renderAccount();

    await user.click(await screen.findByRole("button", { name: "Ada Example" }));
    expect(screen.getByRole("menuitem", { name: "Account" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Admin" })).toBeInTheDocument();
    expect(screen.getByRole("menuitemcheckbox", { name: "Show times in UTC" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Sign out" })).toBeInTheDocument();
    // The theme has its own entry in the sidebar footer.
    expect(screen.queryByRole("menuitem", { name: "Theme" })).not.toBeInTheDocument();
  });

  it("shows the version and a docs link", async () => {
    await renderAccount();

    expect(await screen.findByText(`v${APP_VERSION}`)).toBeInTheDocument();
    const docs = screen.getByRole("link", { name: /Docs/ });
    expect(docs).toHaveAttribute("href", DOCS_URL);
    expect(docs).toHaveAttribute("target", "_blank");
    expect(docs).toHaveAttribute("rel", "noreferrer");
  });

  it("shows only the docs icon when the sidebar is collapsed", async () => {
    await renderAccount({ defaultOpen: false });

    expect(await screen.findByRole("link", { name: "Docs" })).toHaveAttribute("href", DOCS_URL);
    expect(screen.queryByText(`v${APP_VERSION}`)).not.toBeInTheDocument();
  });
});
