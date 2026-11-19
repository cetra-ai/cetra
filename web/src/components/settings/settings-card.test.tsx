import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import { expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SettingsCard, SettingsFormCard } from "./settings-card";

// The form card reads the router for its unsaved-changes guard, so each render sits in one.
async function renderInRouter(ui: ReactNode) {
  const rootRoute = createRootRoute({ component: () => <>{ui}</> });
  const router = createRouter({
    routeTree: rootRoute.addChildren([]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <TooltipProvider>
      <RouterProvider router={router} />
    </TooltipProvider>,
  );
  await act(async () => {
    await router.load();
  });
  return userEvent.setup();
}

type FormProps = Partial<Parameters<typeof SettingsFormCard>[0]>;

function card(overrides: FormProps = {}) {
  return (
    <SettingsFormCard
      title="Project"
      onSubmit={vi.fn()}
      dirty={false}
      pending={false}
      {...overrides}
    >
      <label>
        Name
        <input name="name" />
      </label>
    </SettingsFormCard>
  );
}

it("is a labelled form with its Save in the footer", async () => {
  await renderInRouter(card());

  const form = screen.getByRole("form", { name: "Project" });
  expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
});

it("enables Save once dirty and submits the form", async () => {
  const onSubmit = vi.fn();
  const user = await renderInRouter(card({ dirty: true, onSubmit }));

  expect(screen.getByText("Unsaved changes")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Save" }));
  expect(onSubmit).toHaveBeenCalledTimes(1);
});

it("disables Save while invalid", async () => {
  await renderInRouter(card({ dirty: true, invalid: true }));
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
});

it("disables Save while a save is pending", async () => {
  await renderInRouter(card({ dirty: true, pending: true }));
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
});

it("shows Saved only when the values are not dirty", async () => {
  await renderInRouter(card({ saved: true }));
  expect(screen.getByRole("status")).toHaveTextContent("Saved");
});

it("shows the server error in the footer", async () => {
  await renderInRouter(card({ dirty: true, error: "Could not save" }));
  expect(screen.getByRole("alert")).toHaveTextContent("Could not save");
});

it("explains a role refusal next to the disabled Save", async () => {
  await renderInRouter(card({ dirty: true, disabledReason: "Only owners can edit" }));

  const save = screen.getByRole("button", { name: "Save" });
  expect(save).toBeDisabled();
  expect(save.parentElement).toHaveAccessibleDescription("Only owners can edit");
});

it("renames the save button", async () => {
  await renderInRouter(card({ dirty: true, saveLabel: "Create" }));
  expect(screen.getByRole("button", { name: "Create" })).toBeEnabled();
});

it("labels a plain card by its title", async () => {
  await renderInRouter(
    <SettingsCard title="Retention" description="Days kept.">
      <p>Body</p>
    </SettingsCard>,
  );

  expect(screen.getByRole("region", { name: "Retention" })).toBeInTheDocument();
  expect(screen.getByText("Days kept.")).toBeInTheDocument();
  expect(screen.getByText("Body")).toBeInTheDocument();
});

it("renders the danger tone and footer", async () => {
  await renderInRouter(
    <SettingsCard tone="danger" title="Danger zone" footer={<button type="button">Delete</button>}>
      <p>Body</p>
    </SettingsCard>,
  );

  expect(screen.getByRole("heading", { name: "Danger zone" })).toHaveClass("text-destructive");
  expect(screen.getByRole("button", { name: "Delete" })).toBeInTheDocument();
});
