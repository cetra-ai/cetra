import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { FilterHit } from "@/components/filter-hit";
import { ProjectProvider } from "@/components/shell/project-context";
import projectFixture from "../../../fixtures/api/static/project.json";

afterEach(() => {
  vi.restoreAllMocks();
});

/** Renders the element inside a project route and a memory router, so "Open" items can navigate. */
async function renderInProject(element: React.ReactNode) {
  const rootRoute = createRootRoute({
    component: () => (
      <ProjectProvider
        value={{
          org: "acme",
          project: "support-bot",
          ref: "acme/support-bot",
          data: projectFixture as Schema<"Project">,
        }}
      >
        {element}
      </ProjectProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(<RouterProvider router={router} />);
  await router.load();
  return router;
}

async function openMenuOn(text: string) {
  fireEvent.contextMenu(screen.getByText(text));
  return screen;
}

it("offers filter, exclude and copy for a scalar value", async () => {
  const onFilter = vi.fn();
  await renderInProject(
    <FilterHit
      label="Filter by name checkout"
      field={{ column: "name" }}
      value="checkout"
      onFilter={onFilter}
    >
      checkout
    </FilterHit>,
  );
  await openMenuOn("checkout");
  expect(await screen.findByRole("menuitem", { name: "Filter: name = 'checkout'" })).toBeTruthy();
  fireEvent.click(screen.getByRole("menuitem", { name: "Exclude: name != 'checkout'" }));
  expect(onFilter).toHaveBeenCalledWith("name != 'checkout'");
});

it("filters a list field with contains and offers no exclude", async () => {
  const onFilter = vi.fn();
  await renderInProject(
    <FilterHit
      label="Filter by models gpt-4o"
      field={{ column: "models" }}
      value="gpt-4o"
      list
      onFilter={onFilter}
    >
      gpt-4o
    </FilterHit>,
  );
  await openMenuOn("gpt-4o");
  fireEvent.click(await screen.findByRole("menuitem", { name: "Filter: models contains gpt-4o" }));
  expect(onFilter).toHaveBeenCalledWith("models @> ['gpt-4o']");
  expect(screen.queryByRole("menuitem", { name: /Exclude/ })).toBeNull();
});

it("escapes quotes in the value", async () => {
  const onFilter = vi.fn();
  await renderInProject(
    <FilterHit
      label="Filter by user o'brien"
      field={{ column: "user_id" }}
      value="o'brien"
      onFilter={onFilter}
    >
      o&apos;brien
    </FilterHit>,
  );
  await openMenuOn("o'brien");
  fireEvent.click(await screen.findByRole("menuitem", { name: "Filter: user_id = 'o''brien'" }));
  expect(onFilter).toHaveBeenCalledWith("user_id = 'o''brien'");
});

it("copies the value to the clipboard", async () => {
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  await renderInProject(
    <FilterHit label="Filter by name checkout" field={{ column: "name" }} value="checkout">
      checkout
    </FilterHit>,
  );
  await openMenuOn("checkout");
  fireEvent.click(await screen.findByRole("menuitem", { name: "Copy value" }));
  expect(writeText).toHaveBeenCalledWith("checkout");
});

it("without a filter callback the menu offers copy and open only", async () => {
  await renderInProject(
    <FilterHit
      label="Filter by session s-1"
      field={{ column: "session_id" }}
      value="s-1"
      opens="session"
    >
      s-1
    </FilterHit>,
  );
  await openMenuOn("s-1");
  expect(await screen.findByRole("menuitem", { name: "Copy value" })).toBeTruthy();
  expect(screen.getByRole("menuitem", { name: "Open session" })).toBeTruthy();
  expect(screen.queryByRole("menuitem", { name: /Filter:|Exclude:/ })).toBeNull();
});

it("opens the session page from the menu", async () => {
  const router = await renderInProject(
    <FilterHit
      label="Filter by session s-1"
      field={{ column: "session_id" }}
      value="s-1"
      opens="session"
    >
      s-1
    </FilterHit>,
  );
  await openMenuOn("s-1");
  fireEvent.click(await screen.findByRole("menuitem", { name: "Open session" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions/s-1");
  });
});

it("opens the user page from the menu", async () => {
  const router = await renderInProject(
    <FilterHit
      label="Filter by user user-42"
      field={{ column: "user_id" }}
      value="user-42"
      opens="user"
    >
      user-42
    </FilterHit>,
  );
  await openMenuOn("user-42");
  fireEvent.click(await screen.findByRole("menuitem", { name: "Open user" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/users/user-42");
  });
});

it("keeps menu clicks from reaching the row around the cell", async () => {
  const onRowClick = vi.fn();
  const onFilter = vi.fn();
  await renderInProject(
    // biome-ignore lint/a11y/noStaticElementInteractions: a stand-in for the table row that the menu must not reach
    // biome-ignore lint/a11y/useKeyWithClickEvents: a stand-in for the table row that the menu must not reach
    <div onClick={onRowClick}>
      <FilterHit
        label="Filter by name checkout"
        field={{ column: "name" }}
        value="checkout"
        onFilter={onFilter}
      >
        checkout
      </FilterHit>
    </div>,
  );
  await openMenuOn("checkout");
  fireEvent.click(await screen.findByRole("menuitem", { name: "Exclude: name != 'checkout'" }));
  expect(onFilter).toHaveBeenCalledWith("name != 'checkout'");
  expect(onRowClick).not.toHaveBeenCalled();
});

it("gives long menu items a truncated label with the full text as title", async () => {
  const value = "x".repeat(200);
  await renderInProject(
    <FilterHit
      label={`Filter by name ${value}`}
      field={{ column: "name" }}
      value={value}
      onFilter={vi.fn()}
    >
      short
    </FilterHit>,
  );
  fireEvent.contextMenu(screen.getByText("short"));
  const item = await screen.findByRole("menuitem", { name: `Filter: name = '${value}'` });
  expect(item.className).toContain("truncate");
  expect(item.getAttribute("title")).toBe(`Filter: name = '${value}'`);
});
