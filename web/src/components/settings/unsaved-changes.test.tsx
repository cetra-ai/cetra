import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  Link,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { expect, it } from "vitest";
import { UnsavedChangesGuard } from "./unsaved-changes";

function Editor() {
  const [name, setName] = useState("");
  return (
    <>
      <label>
        Name
        <input
          value={name}
          onChange={(event) => {
            setName(event.target.value);
          }}
        />
      </label>
      <UnsavedChangesGuard dirty={name !== ""} />
      <Link to="/account">Other page link</Link>
      <p>Editing page</p>
    </>
  );
}

function renderRouter() {
  const rootRoute = createRootRoute();
  const editRoute = createRoute({ getParentRoute: () => rootRoute, path: "/", component: Editor });
  const otherRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/account",
    component: () => <p>Other page</p>,
  });
  const router = createRouter({
    routeTree: rootRoute.addChildren([editRoute, otherRoute]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(<RouterProvider router={router} />);
  return { router, user: userEvent.setup() };
}

it("navigates without asking while nothing is dirty", async () => {
  const { user } = renderRouter();
  await screen.findByText("Editing page");

  await user.click(screen.getByRole("link", { name: "Other page link" }));
  expect(await screen.findByText("Other page")).toBeInTheDocument();
  expect(screen.queryByRole("alertdialog")).toBeNull();
});

it("asks before leaving dirty edits, and keeps editing on Keep editing", async () => {
  const { router, user } = renderRouter();
  await user.type(await screen.findByLabelText("Name"), "bot");

  await user.click(screen.getByRole("link", { name: "Other page link" }));

  const dialog = await screen.findByRole("alertdialog", { name: "Discard unsaved changes?" });
  expect(dialog).toHaveTextContent("Your edits are not saved.");

  await user.click(screen.getByRole("button", { name: "Keep editing" }));
  expect(screen.queryByRole("alertdialog")).toBeNull();
  expect(router.state.location.pathname).toBe("/");
  expect(screen.getByLabelText("Name")).toHaveValue("bot");
});

it("leaves the page on Discard", async () => {
  const { router, user } = renderRouter();
  await user.type(await screen.findByLabelText("Name"), "bot");

  await user.click(screen.getByRole("link", { name: "Other page link" }));
  await user.click(await screen.findByRole("button", { name: "Discard" }));

  expect(await screen.findByText("Other page")).toBeInTheDocument();
  expect(router.state.location.pathname).toBe("/account");
});
