import { screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { renderApp } from "@/test/render";

it("shows the user's id, an Open page link and the metadata", async () => {
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  expect(within(panel).getByText("user-42")).toBeInTheDocument();
  expect(within(panel).getByRole("link", { name: "Open page" })).toHaveAttribute(
    "href",
    "/acme/support-bot/users/user-42",
  );
  await waitFor(() => {
    expect(within(panel).getByText("Ada Example")).toBeInTheDocument();
  });
  expect(within(panel).getByText("name")).toBeInTheDocument();
  expect(within(panel).getByText("Recent sessions")).toBeInTheDocument();
});

it("removes the user from the URL when Close is clicked", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  await user.click(within(panel).getByRole("button", { name: "Close" }));
  await waitFor(() => {
    expect(router.state.location.search.user).toBeUndefined();
  });
});
