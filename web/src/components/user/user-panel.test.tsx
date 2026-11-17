import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import userFixture from "../../../../fixtures/api/responses/users/user-42.json";

it("shows the user's id, an Open page link and the metadata", async () => {
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  expect(within(panel).getByText("user-42")).toBeInTheDocument();
  expect(within(panel).getByRole("link", { name: "Open page" })).toHaveAttribute(
    "href",
    "/acme/support-bot/users/user-42",
  );
  await waitFor(() => {
    const header = panel.querySelector("header") as HTMLElement;
    expect(within(header).getByText("Ada Example")).toBeInTheDocument();
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

it("shows the name with the id after it, the plan pill, the chips and the activity chart", async () => {
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  await waitFor(() => {
    expect(within(panel).getByRole("img", { name: "Activity" })).toBeInTheDocument();
  });
  const header = panel.querySelector("header");
  expect(header).not.toBeNull();
  expect(within(header as HTMLElement).getByText("Ada Example")).toBeInTheDocument();
  expect(within(header as HTMLElement).getByText("user-42")).toBeInTheDocument();
  expect(within(header as HTMLElement).getByText("pro")).toBeInTheDocument();
  expect(within(panel).getByText("1 session")).toBeInTheDocument();
  expect(within(panel).getByText("3 traces")).toBeInTheDocument();
});

it("links a recent session with its first message, and marks a session with errors", async () => {
  const sessions = userFixture.sessions.map((session) => ({
    ...session,
    first_input_preview: "My order 1234 hasn't arrived.",
    error_trace_count: 1,
  }));
  server.use(
    http.get("/api/v1/projects/:project/users/:user_id", () =>
      HttpResponse.json({ ...userFixture, sessions }),
    ),
  );
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  const link = await within(panel).findByRole("link", { name: /My order 1234/ });
  expect(link).toHaveAttribute("href", "/acme/support-bot/sessions/sess-001");
  expect(within(link).getByText("(has errors)", { exact: false })).toBeInTheDocument();
  expect(link).toHaveAccessibleName(/^My order 1234 hasn't arrived\.\s*\(has errors\)/);
});

it("shows the session id when a session has no first message and no errors", async () => {
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  const link = await within(panel).findByRole("link", { name: /sess-001/ });
  expect(link).toHaveAttribute("href", "/acme/support-bot/sessions/sess-001");
  expect(within(link).queryByText("(has errors)", { exact: false })).not.toBeInTheDocument();
  expect(link).toHaveAccessibleName(/^sess-001/);
});
